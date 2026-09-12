using Avalonia;
using Avalonia.Controls;
using Avalonia.Interactivity;
using Avalonia.Threading;
using Avalonia.VisualTree;
using System.Text.Json;
using GnomeAI.Client;

namespace GnomeAI.UI;

public sealed partial class MainWindow
{
    private DeviceHub? _devices;
    private PeerLink? _selectedPeer;
    private DeviceSessionClient? _remoteSource;
    private bool _physicalConnected, _updatingDevicePicker, _changingSource, _remoteSessionActive=true;
    private string _lastLocalSession="";
    private readonly Dictionary<string,string> _lastPeerSessions=[];
    private int _sourceGeneration;
    private bool _refreshAfterSelection;
    private System.Threading.Timer? _networkDebounce;
    private readonly Dictionary<string,string> _capabilities=[];
    private readonly HashSet<string> _capabilitiesLoading=[];
    private void DeviceNetworkChanged(object? sender,EventArgs args)=>_networkDebounce?.Change(1000,Timeout.Infinite);
    private StackPanel? _deviceCards;
    private Avalonia.Media.Imaging.Bitmap? _invitationBitmap;
    private sealed record DeviceChoice(string Label,PeerLink? Link,bool Manage=false) {public override string ToString()=>Label;}
    private string RuntimeKey(string id)=> (_selectedPeer is null?"":_selectedPeer.Peer.Id+":")+(id.Length==0?"__pending__":id);
    private void InitializeDevices()
    {
        var home=Environment.GetEnvironmentVariable("GNOMEF_RS_HOME")
            ?? Path.Combine(Environment.GetEnvironmentVariable("XDG_STATE_HOME")
                ?? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile),".local","state"),"gnomeai-rs");
        _devices=new DeviceHub(_bridge,Path.Combine(home,"store","devices"),Environment.MachineName);
        _devices.Changed+=DevicesChanged;
        _networkDebounce=new System.Threading.Timer(_=>_devices?.ReconnectAll(),null,Timeout.Infinite,Timeout.Infinite);
        System.Net.NetworkInformation.NetworkChange.NetworkAddressChanged+=DeviceNetworkChanged;
        DevicePicker.SelectionChanged+=async(_,_)=> {
            if(_updatingDevicePicker)return;
            await RunUiAsync(async()=> {
                var choice=DevicePicker.SelectedItem as DeviceChoice;
                if(choice?.Manage==true){ShowDevicesPage();RefreshDevicePicker();}
                else await SelectSourceAsync(choice?.Link);
            });
        };
        RefreshDevicePicker();
    }
    private void DevicesChanged()=>Dispatcher.UIThread.Post(()=>{
        if(_closed)return;
        if(_selectedPeer is {} removed && _devices is not null && !_devices.Links.Contains(removed)) {
            _=RunUiAsync(()=>SelectSourceAsync(null));
        }
        var liveIds=_devices?.Links.Select(l=>l.Peer.Id).ToHashSet()??[];
        foreach(var id in _lastPeerSessions.Keys.Where(id=>!liveIds.Contains(id)).ToArray()) {
            _lastPeerSessions.Remove(id);
            foreach(var key in _sessionRuntimes.Keys.Where(key=>key.StartsWith(id+":",StringComparison.Ordinal)).ToArray()) {
                _dirtyDraftSessions.Remove(key);_sessionRuntimes.Remove(key);
                try{_draftStore.Save(key,new(ComposerDraft.Empty,[]));}catch(Exception error){ShowNotice("Could not remove cached draft: "+error.Message);}
            }
        }
        RefreshDevicePicker();RefreshDeviceCards();
        if(_selectedPeer is {} peer) {
            _coreConnected=peer.Online;
            if(!peer.Online)RuntimeFor(_currentSessionId).QueuePaused=true;
            RefreshHeader();RefreshComposer();
        }
    });
    private void RefreshDevicePicker() {
        if(_devices is null)return;
        _updatingDevicePicker=true;
        var choices=new List<DeviceChoice>{new("This PC",null)};
        choices.AddRange(_devices.Links.Where(l=>l.Peer.Trusted).Select(l=>new DeviceChoice(l.Peer.Name+" · "+l.ConnectionLabel,l)));
        choices.Add(new("Manage devices…",null,true));
        DevicePicker.ItemsSource=choices;DevicePicker.SelectedItem=choices.FirstOrDefault(c=>!c.Manage && c.Link==_selectedPeer)??choices[0];
        _updatingDevicePicker=false;RefreshSourceControls();
    }
    private void RefreshSourceControls() {
        if(_devices is null)return;
        DevicePicker.IsEnabled=!_changingSource && (_selectedPeer is not null || (!_dispatching && !_sessionTransitioning));
        MoveSessionButton.IsVisible=_currentSessionId.Length>0 && (_selectedPeer?.Online==true || _devices.Links.Any(l=>l.Online));
        ProviderButton.IsEnabled=SearchButton.IsEnabled=WorkspaceButton.IsEnabled=ModelButton.IsEnabled=_selectedPeer is null;
        AttachButton.IsEnabled=_selectedPeer is null;
        if(_selectedPeer is {} peer) {
            ConnectionText.Text=peer.Peer.Name+" · "+peer.ConnectionLabel;
            WorkspaceText.Text=peer.Peer.Name+(_workspace.Length>0?" · "+_workspace:"")+(!_remoteSessionActive?" · Execution moved":"");
            SendButton.IsEnabled=SendButton.IsEnabled && _remoteSessionActive && _currentSessionId.Length>0;
        }
    }
    private void ShowConversationPage() {DevicesPage.IsVisible=false;ChatWorkspace.IsVisible=true;}
    private async Task SelectSourceAsync(PeerLink? peer,string? session=null) {
        if(_changingSource || (_selectedPeer is null && (_dispatching || _sessionTransitioning))){RefreshDevicePicker();return;}
        ShowConversationPage();
        if(peer==_selectedPeer)return;
        _changingSource=true;RefreshSourceControls();
        try {
        CaptureComposerDraft();SaveComposerDrafts();RuntimeFor(_currentSessionId).QueuePaused=true;
        if(_selectedPeer is null)_lastLocalSession=_currentSessionId;
        else _lastPeerSessions[_selectedPeer.Peer.Id]=_remoteSource?.SessionId??_currentSessionId;
        var previous=_remoteSource;_remoteSource=null;
        var generation=++_sourceGeneration;_refreshAfterSelection=false;
        // Cancels subscriptions and read requests immediately; navigation never
        // waits for the peer's 120-second request deadline.
        if(previous is not null)_=RetireSourceAsync(previous);
        _selectedPeer=peer;_currentSessionId="";_sessionTransitioning=false;_dispatching=false;
        _sessionRows.Clear();_attachments.Clear();Composer.Text="";_workspace="";_model="";_providerName="";_gitBranch=null;
        _coreConnected=peer?.Online??_physicalConnected;_remoteSessionActive=true;
        ResetTranscript();RefreshSessionFilter();RefreshDevicePicker();RefreshHeader();
        if(peer is null) {
            _lastLocalSession=session??_lastLocalSession;
            if(_physicalConnected) {
                await SendAsync(new() { ["op"]="list_sessions" });
                await SendAsync(new() { ["op"]=_lastLocalSession.Length>0?"resume_session":"new_session",["id"]=_lastLocalSession });
            }
            return;
        }
        var source=new DeviceSessionClient(_devices!,peer);_remoteSource=source;
        bool Current()=>!_closed && generation==_sourceGeneration && _remoteSource==source;
        source.SessionsReceived+=data=>Dispatcher.UIThread.Post(()=>{
            if(!Current())return;
            RenderRemoteList(data);
        });
        source.SnapshotReceived+=(id,data)=>Dispatcher.UIThread.Post(async()=>{
            if(Current() && source.SessionId==id){_lastPeerSessions[peer.Peer.Id]=id;await RunUiAsync(()=>RenderDeviceSnapshotAsync(id,data));}
        });
        source.SessionEventReceived+=data=>Dispatcher.UIThread.Post(async()=>{
            if(Current())await RunUiAsync(()=>HandleSessionEventAsync(data));
        });
        source.Error+=message=>Dispatcher.UIThread.Post(()=>{if(Current())ConnectionText.Text=peer.Online?message:peer.Peer.Name+" · "+peer.ConnectionLabel;});
        source.Start();
        var remembered=session??_lastPeerSessions.GetValueOrDefault(peer.Peer.Id,"");
        if(remembered.Length>0)_=RestoreRemoteSessionAsync(source,remembered,generation);
        } finally {_changingSource=false;RefreshDevicePicker();}
    }
    private static async Task RetireSourceAsync(DeviceSessionClient source) {
        try{await source.DisposeAsync();}catch(OperationCanceledException){}
    }
    private async Task RestoreRemoteSessionAsync(DeviceSessionClient source,string id,int generation) {
        try{await source.SelectSessionAsync(id);}
        catch(OperationCanceledException){}
        catch(Exception error){if(!_closed && _remoteSource==source && _sourceGeneration==generation)ShowNotice(error.Message);}
    }
    private void RenderRemoteList(JsonElement data) {
        _sessionRows.Clear();
        foreach(var item in data.GetProperty("sessions").EnumerateArray()) {
            var id=String(item,"id");
            _sessionRows.Add(new SessionItem {Id=id,Title=String(item,"title","Conversation"),Model=String(item,"model"),
                Project=_selectedPeer!.Peer.Name,IsCurrent=id==_currentSessionId,
                UpdatedAt=item.TryGetProperty("updated_at",out var updated)?updated.GetInt64():0,
                IsBusy=Bool(item,"busy"),NeedsAttention=RuntimeFor(id).NeedsAttention});
        }
        RefreshSessionFilter();RefreshHeader();
    }
    private bool SelectingDesktopResponse()=>TranscriptContent.GetVisualDescendants().OfType<MarkdownView>().Any(view=>view.IsSelectingText);
    private async void ResponseSelectionChanged(object? sender,EventArgs args) {
        if(!_refreshAfterSelection || SelectingDesktopResponse() || _remoteSource is not {} source)return;
        _refreshAfterSelection=false;
        await RunUiAsync(async()=>{try{await source.RefreshSnapshotAsync();}catch(OperationCanceledException){}});
    }
    private async Task RenderDeviceSnapshotAsync(string id,JsonElement snapshot) {
        if(id==_currentSessionId && SelectingDesktopResponse()){_refreshAfterSelection=true;return;}
        CaptureComposerDraft();SaveComposerDrafts();SwitchComposerSession(id);ResetTranscript();
        _sessionTransitioning=false;
        if(snapshot.TryGetProperty("session",out var session)) {
            _workspace=String(session,"workspace");_model=String(session,"model");
            _remoteSessionActive=String(session,"status","active")=="active";
        }
        foreach(var row in _sessionRows)row.IsCurrent=row.Id==id;
        await HandleEventAsync(JsonSerializer.SerializeToElement(new { @event="history_replay",turns=snapshot.GetProperty("turns") }));
        if(snapshot.TryGetProperty("live",out var live) && live.ValueKind==JsonValueKind.Object) {
            var reasoning=String(live,"reasoning");var text=String(live,"text");
            if(reasoning.Length>0)await HandleEventAsync(JsonSerializer.SerializeToElement(new { @event="reasoning",text=reasoning }));
            if(text.Length>0)await HandleEventAsync(JsonSerializer.SerializeToElement(new { @event="token",text }));
        }
        if(snapshot.TryGetProperty("approvals",out var approvals))foreach(var approval in approvals.EnumerateArray())AddApprovalCard(approval);
        _busy=Bool(snapshot,"busy");RuntimeFor(id).Busy=_busy;
        RefreshSessionFilter();RefreshHeader();RefreshComposer();
    }
    private async Task SendRemoteAsync(Dictionary<string,object?> op) {
        var source=_remoteSource??throw new IOException("Choose a device first.");
        var generation=_sourceGeneration;
        bool Current()=>_remoteSource==source && generation==_sourceGeneration;
        var navigates=op.GetValueOrDefault("op") is "resume_session" or "new_session";
        if(navigates && _sessionTransitioning)throw new IOException("A conversation is still opening. Please wait.");
        if(navigates){CaptureComposerDraft();SaveComposerDrafts();_sessionTransitioning=true;RefreshComposer();}
        try {
        string Value(string name)=>op.TryGetValue(name,out var value)?value?.ToString()??"":"";
        if(navigates)ShowConversationPage();
        switch(Value("op")) {
            case "list_sessions": await source.RefreshListAsync();break;
            case "resume_session": await source.SelectSessionAsync(Value("id"));break;
            case "new_session": await source.NewAsync();break;
            case "submit": await source.RequestAsync("submit",new {session_id=_currentSessionId,text=Value("text")});break;
            case "interrupt": await source.RequestAsync("interrupt",new {session_id=_currentSessionId});break;
            case "approve": await source.RequestAsync("approve",new {session_id=_currentSessionId,call_id=Value("call_id"),decision=Value("decision")});break;
            case "rename_session": await source.RequestAsync("rename",new {session_id=Value("id"),title=Value("title")});await source.RefreshListAsync();break;
            case "delete_session":
                await source.RequestAsync("delete",new {session_id=Value("id")});
                if(!Current())return;
                if(Value("id")==_currentSessionId){CaptureComposerDraft();SaveComposerDrafts();_currentSessionId="";await source.SelectSessionAsync("");ResetTranscript();}
                await source.RefreshListAsync();break;
            default: throw new InvalidOperationException("This action is available on This PC. Select This PC to use it.");
        }
    }
        catch(Exception) when(!Current()) { /* The user changed source; old replies cannot alter the new context. */ }
        finally {if(Current() && navigates){_sessionTransitioning=false;RefreshComposer();}}
    }
    private void ShowDevicesPage() {
        if(_devices is null){ShowNotice("Start the Rust core to manage devices.");return;}
        ChatWorkspace.IsVisible=false;ActivityPane.IsVisible=false;DevicesPage.IsVisible=true;
        _invitationBitmap?.Dispose();_invitationBitmap=null;
        var body=new StackPanel {Spacing=14,Margin=new Thickness(24),MaxWidth=900};
        body.Children.Add(DialogHeading("Devices","Pair a phone or desktop, or manage an execution node. Compare the six digits on both screens before confirming."));
        var code=new TextBox {Watermark="Paste pairing code",AcceptsReturn=true,TextWrapping=Avalonia.Media.TextWrapping.Wrap,MaxHeight=130};
        var qr=new Image {MaxWidth=440,MaxHeight=440};
        var feedback=MutedText("Invitations expire after ten minutes.");
        var create=ActionButton("Create invitation",async()=>{
            feedback.Text="Connecting to Tor…";
            code.Text=await _devices.CreatePairingAsync("tor");
            _invitationBitmap?.Dispose();_invitationBitmap=CreateQrBitmap(code.Text);qr.Source=_invitationBitmap;
            feedback.Text="Scan with GnomeAI on the other device, then compare the six digits.";
        });
        body.Children.Add(ButtonRow(create,ActionButton("Pair new device",()=>{_devices.Pair((code.Text??"").Trim());feedback.Text="Connecting… Keep both devices open.";return Task.CompletedTask;}),
            ActionButton("Copy invitation",()=>CopyTextAsync(code.Text??""))));
        body.Children.Add(code);body.Children.Add(qr);
        body.Children.Add(ActionButton("Enlarge QR",async()=>{
            if(string.IsNullOrEmpty(code.Text))return;
            using var bitmap=CreateQrBitmap(code.Text);
            var dialog=CreateDialog("Scan pairing invitation",740,800);
            dialog.Content=new Image {Source=bitmap,Margin=new Thickness(24),Stretch=Avalonia.Media.Stretch.Uniform};
            await dialog.ShowDialog(this);
        }));
        body.Children.Add(feedback);
        if(_devices.HasPendingTransfer)body.Children.Add(ActionButton("Resume pending transfer",()=>_devices.ResumeMoveAsync()));
        _deviceCards=new StackPanel {Spacing=10};body.Children.Add(_deviceCards);RefreshDeviceCards();
        var nodes=new StackPanel {Spacing=10};
        async Task RefreshNodes() {
            nodes.Children.Clear();
            if(_whatsapp?.NodeEnabled!=true){nodes.Children.Add(MutedText("Execution node listener is disabled in local Settings."));return;}
            try {
                var data=await NodeRequestAsync(HttpMethod.Get,"/v1/nodes");
                if(data is {} payload && payload.TryGetProperty("nodes",out var list))foreach(var node in list.EnumerateArray())nodes.Children.Add(CreateNodeCard(NodeInfo.FromJson(node)));
            }catch(Exception error){nodes.Children.Add(MutedText(error.Message));}
        }
        body.Children.Add(Section("EXECUTION NODES"));
        body.Children.Add(MutedText("These devices execute tools; phone and desktop peers also store conversations and support Move / Sync."));
        body.Children.Add(ActionButton("Refresh devices",RefreshNodes));body.Children.Add(nodes);
        if(_whatsapp is {} cfg)body.Children.Add(new Expander {Header="Enroll an execution node",Content=ActionButton("Copy enrollment command",()=>CopyTextAsync($"gnomeai-node enroll --server http://PC-IP:{cfg.NodePort} --token {cfg.NodeEnrollmentToken} --name NAME"))});
        DevicesPage.Content=new ScrollViewer {Content=body};_ = RunUiAsync(RefreshNodes);
    }
    private void RefreshDeviceCards() {
        if(_deviceCards is null || _devices is null)return;
        _deviceCards.Children.Clear();
        foreach(var link in _devices.Links) {
            var expired=!link.Peer.Trusted && link.Peer.Expires<DateTimeOffset.UtcNow.ToUnixTimeSeconds();
            var card=new StackPanel {Spacing=8};
            card.Children.Add(new TextBlock {Text=link.Peer.Name+" · "+link.ConnectionLabel,FontSize=18});
            card.Children.Add(MutedText(_capabilities.GetValueOrDefault(link.Peer.Channel,"Conversations · encrypted transport · workspace Move / Sync")));
            if(link.Online && !_capabilities.ContainsKey(link.Peer.Channel) && _capabilitiesLoading.Add(link.Peer.Channel))_ = LoadCapabilitiesAsync(link);
            if(link.AwaitingConfirmation) {
                card.Children.Add(new TextBlock {Text=link.SecurityCode,FontSize=30});
                if(!link.Peer.LocalConfirmed)card.Children.Add(ActionButton("Digits match · Confirm",()=>link.ConfirmAsync()));
                else card.Children.Add(MutedText("Waiting for confirmation on the other device…"));
            }
            if(link.Peer.Trusted)card.Children.Add(ActionButton("Open conversations",()=>SelectSourceAsync(link)));
            card.Children.Add(ActionButton(link.Peer.Trusted?"Forget device":"Cancel invitation",async()=>{
                if(!await ConfirmAsync("Forget device",$"Remove pairing and cached drafts for {link.Peer.Name}? The other device will be notified if connected."))return;
                if(_selectedPeer==link)await SelectSourceAsync(null);
                await _devices.ForgetAsync(link);
                _capabilities.Remove(link.Peer.Channel);_capabilitiesLoading.Remove(link.Peer.Channel);
                RefreshDeviceCards();ShowNotice(_devices.SyncStatus);
            }));
            _deviceCards.Children.Add(Surface(card));
        }
    }
    private async Task LoadCapabilitiesAsync(PeerLink peer) {
        try {
            var data=await _devices!.RequestAsync(peer,"capabilities",new {});
            if(_devices is null || !_devices.Links.Contains(peer))return;
            _capabilities[peer.Peer.Channel]=String(data,"platform","Device")+" · Conversations · Move / Sync"+(Bool(data,"shell")?" · Shell":"")+(Bool(data,"desktop_control")?" · Desktop control":"");
            RefreshDeviceCards();
        }catch(Exception){ /* A later connection state update will retry. */ }
        finally{_capabilitiesLoading.Remove(peer.Peer.Channel);}
    }
    private void CacheLocalEvent(JsonElement frame) {
        if(!frame.TryGetProperty("session_id",out var session) || !frame.TryGetProperty("payload",out var payload))return;
        var id=session.GetString()??"";if(id.Length==0)return;
        if(!_sessionRuntimes.TryGetValue(id,out var runtime)){runtime=new SessionRuntime();_sessionRuntimes[id]=runtime;}
        var kind=String(payload,"event");
        if(kind=="turn_started"){runtime.Busy=true;runtime.LiveEvents.Clear();runtime.NeedsAttention=false;}
        if(runtime.Busy)BufferLiveEvent(runtime,payload,kind);
        if(kind is "approval_request" or "privilege_credential_request")runtime.NeedsAttention=true;
        if(kind is "turn_completed" or "interrupted" or "error"){runtime.Busy=false;runtime.LiveEvents.Clear();runtime.NeedsAttention=true;runtime.QueuePaused=true;}
    }
    private void Devices_Click(object? sender,RoutedEventArgs e)=>ShowDevicesPage();
    private async void MoveDevice_Click(object? sender,RoutedEventArgs e)=>await RunUiAsync(async()=>{
        if(_devices is null || _currentSessionId.Length==0)return;
        var source=_selectedPeer;var session=_currentSessionId;
        var dialog=CreateDialog("Session devices",560,420);
        var body=new StackPanel {Spacing=12,Margin=new Thickness(22)};
        body.Children.Add(DialogHeading("Move / Sync","Move execution with its workspace, or synchronize files for this conversation."));
        foreach(var peer in source is null?_devices.Links.Where(p=>p.Online):new[]{source}) {
            var target=source is null?peer.Peer.Name:"This PC";
            body.Children.Add(ActionButton("Move execution to "+target,async()=>{
                await _devices.MoveAsync(peer,session,source is not null);dialog.Close();
                await SelectSourceAsync(source is null?peer:null,session);
            }));
            body.Children.Add(ActionButton("Sync files with "+peer.Peer.Name,async()=>{await _devices.SyncWorkspaceAsync(peer,session);ShowNotice(_devices.SyncStatus);}));
            body.Children.Add(ActionButton("Pause sync with "+peer.Peer.Name,()=>{_devices.DisableSync(peer,session);return Task.CompletedTask;}));
        }
        dialog.Content=new ScrollViewer {Content=body};await dialog.ShowDialog(this);
    });
}
