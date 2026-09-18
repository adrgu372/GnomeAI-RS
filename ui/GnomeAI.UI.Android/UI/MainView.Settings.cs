using System.Text.Json;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;
using CheckBox=Avalonia.Controls.CheckBox;

namespace GnomeAI.Android.UI;

public sealed partial class MainView
{
    private int _settingsGeneration;
    private string[] _availableModels=[];
    private string _availableActiveModel="";
    private TaskCompletionSource<JsonElement>? _providerUpdate;
    private readonly SemaphoreSlim _providerChange=new(1,1);
    private void AcceptModelUpdate(JsonElement message)
    {
        if(message.TryGetProperty("models",out var list))_availableModels=list.EnumerateArray().Select(m=>m.GetString()??"").Where(m=>m.Length>0).Distinct().ToArray();
        if(message.TryGetProperty("model",out var model))_availableActiveModel=model.GetString()??"";
        if(message.GetProperty("event").GetString()=="provider_changed")_providerUpdate?.TrySetResult(message.Clone());
    }
    private async Task LocalOp(string op,params (string Key,object? Value)[] pairs)
    {
        var payload=new Dictionary<string,object?> {["op"]=op};
        foreach(var pair in pairs)payload[pair.Key]=pair.Value;
        await _hub.Bridge.SendAsync(payload);
    }
    private async Task ShowSettingsAsync()
    {
        var settings=await _hub.RequestAsync(null,"settings",new {});
        if(_disposed)return;
        string Setting(string name)=>settings.GetProperty(name).GetString()??"";
        ShowPanel();SelectTab("Settings");var generation=_settingsGeneration;
        bool Current()=>!_disposed && generation==_settingsGeneration;
        _panel.Children.Add(new TextBlock {Text="Settings",FontSize=24,FontWeight=FontWeight.SemiBold});
        _panel.Children.Add(Button("Skills · install SKILL.md", ShowSkillsAsync));
        var theme=new ComboBox {ItemsSource=new[]{"System","Light","Dark"},SelectedItem=MobileAppearance.Preference switch{"dark"=>"Dark","light"=>"Light",_=>"System"},HorizontalAlignment=HorizontalAlignment.Stretch};
        theme.SelectionChanged+=(_,_)=>MobileAppearance.SetTheme((theme.SelectedItem?.ToString()??"System").ToLowerInvariant());
        AddSettingsGroup("Appearance",SettingField("Theme",theme));
        var config=_config.ValueKind==JsonValueKind.Object?_config:_hub.Config;
        var catalog=config.ValueKind==JsonValueKind.Object && config.TryGetProperty("providers",out var list)?list.EnumerateArray().ToArray():[];
        var providers=catalog.Length>0?catalog.Select(p=>p.GetProperty("id").GetString()!).ToArray():new[]{"openai","anthropic","custom"};
        var savedProvider=Setting("provider_id");
        var provider=new ComboBox {ItemsSource=providers,SelectedIndex=Math.Max(0,Array.IndexOf(providers,savedProvider)),HorizontalAlignment=HorizontalAlignment.Stretch};
        var models=new ComboBox {MaxDropDownHeight=280,ItemsSource=_availableModels,SelectedItem=Setting("model"),HorizontalAlignment=HorizontalAlignment.Stretch};
        var model=new TextBox {Watermark="Model ID (or choose above)",Text=Setting("model")};
        var key=new TextBox {Watermark="API key (blank keeps saved key)",PasswordChar='●'};
        var url=new TextBox {Watermark="API base URL (optional)",Text=savedProvider=="custom"?Setting("base_url"):""};
        var modelStatus=new TextBlock {Text=_availableModels.Length>0?"Choose a model, or enter its ID.":"Save the provider configuration to load available models.",TextWrapping=TextWrapping.Wrap,FontSize=12};
        models.SelectionChanged+=(_,_)=>{if(models.SelectedItem is string id)model.Text=id;};
        provider.SelectionChanged+=(_,_)=>{
            models.ItemsSource=Array.Empty<string>();model.Text="";key.Text="";url.Text="";
            modelStatus.Text="Save this provider to fetch its available models.";
            var selected=catalog.FirstOrDefault(p=>p.GetProperty("id").GetString()==provider.SelectedItem?.ToString());
            if(selected.ValueKind==JsonValueKind.Object && selected.TryGetProperty("default_model",out var fallback))model.Watermark="Model ID · default "+fallback.GetString();
        };
        var reasoning=new ComboBox {ItemsSource=new[]{"default","low","medium","high","xhigh"},SelectedItem=Setting("reasoning")};
        var web=new CheckBox {Content="Enable web search",IsChecked=settings.GetProperty("web").GetBoolean()};
        var brave=new TextBox {Watermark="Brave API key (blank keeps saved key)",PasswordChar='●'};
        var memory=new CheckBox {Content="Enable memory",IsChecked=settings.GetProperty("memory").GetBoolean()};
        var workers=new CheckBox {Content="Use a separate provider/model for subagents",IsChecked=settings.GetProperty("workers").GetBoolean()};
        var workerProviders=catalog.Length>0?catalog.Where(p=>!p.TryGetProperty("auth",out var auth) || auth.GetString()!="account").Select(p=>p.GetProperty("id").GetString()!).Prepend("inherit").Distinct().ToArray():providers.Prepend("inherit").ToArray();
        var workerProvider=new ComboBox {ItemsSource=workerProviders,SelectedIndex=Math.Max(0,Array.IndexOf(workerProviders,Setting("worker_provider")))};
        var workerModel=new TextBox {Watermark="Subagent model ID",Text=Setting("worker_model")};
        var workerModels=new ComboBox {MaxDropDownHeight=280,HorizontalAlignment=HorizontalAlignment.Stretch};
        var workerStatus=new TextBlock {TextWrapping=TextWrapping.Wrap,FontSize=12};
        var workerRequest=0;
        async Task LoadWorkerModels() {
            var request=++workerRequest;var selected=workerProvider.SelectedItem?.ToString()??savedProvider;
            workerModels.ItemsSource=Array.Empty<string>();workerStatus.Text="Loading available models…";
            try {
                var result=await _hub.RequestAsync(null,"available_models",new {provider_id=selected=="inherit"?savedProvider:selected});
                if(!Current() || request!=workerRequest)return;
                workerModels.ItemsSource=result.GetProperty("models").EnumerateArray().Select(x=>x.GetString()??"").ToArray();
                workerModels.SelectedItem=workerModel.Text;workerStatus.Text="Choose a model or enter its ID.";
            } catch(Exception error) {if(Current() && request==workerRequest)workerStatus.Text="Model list unavailable: "+error.Message;}
        }
        workerModels.SelectionChanged+=(_,_)=>{if(workerModels.SelectedItem is string id)workerModel.Text=id;};
        workerProvider.SelectionChanged+=async(_,_)=>{workerModel.Text="";await LoadWorkerModels();};
        _=LoadWorkerModels();
        var workerReasoning=new ComboBox {ItemsSource=new[]{"default","low","medium","high","xhigh"},SelectedItem=Setting("worker_reasoning")};
        AddSettingsGroup("Model & provider",SettingField("Provider",provider),key,url,SettingField("Available models",models),model,modelStatus,SettingField("Reasoning",reasoning));
        AddSettingsGroup("Web & memory",web,brave,memory);
        AddSettingsGroup("Subagents",workers,SettingField("Provider",workerProvider),SettingField("Available models",workerModels),workerModel,workerStatus,SettingField("Reasoning",workerReasoning));
        _panel.Children.Add(Button("Save",async()=>{
            var providerId=provider.SelectedItem?.ToString()??savedProvider;
            var requestedModel=model.Text?.Trim()??"";
            // Capture the credentials and model together; avoid reading a different form after awaits.
            var apiKey=key.Text;var baseUrl=url.Text;
            modelStatus.Text="Saving provider and loading available models…";models.IsEnabled=false;provider.IsEnabled=key.IsEnabled=url.IsEnabled=model.IsEnabled=false;
            await _providerChange.WaitAsync();
            try {
                var completion=new TaskCompletionSource<JsonElement>(TaskCreationOptions.RunContinuationsAsynchronously);
                _providerUpdate=completion;
                await LocalOp("set_provider",("provider_id",providerId),("api_key",string.IsNullOrWhiteSpace(apiKey)?null:apiKey),("base_url",string.IsNullOrWhiteSpace(baseUrl)?null:baseUrl));
                var update=await completion.Task.WaitAsync(TimeSpan.FromSeconds(60));
                var available=update.GetProperty("models").EnumerateArray().Select(m=>m.GetString()??"").Where(m=>m.Length>0).Distinct().ToArray();
                var chosen=requestedModel.Length>0?requestedModel:update.GetProperty("model").GetString()??"";
                if(chosen.Length>0)await LocalOp("set_model",("model",chosen));
                if(Current()){
                    models.ItemsSource=available;models.SelectedItem=chosen;model.Text=chosen;
                    modelStatus.Text=available.Length>0?$"{available.Length} models available. Provider saved.":"Provider saved. No model list was returned; enter a model ID.";
                    key.Text="";savedProvider=providerId;await LoadWorkerModels();
                }
            } catch(Exception) {
                if(Current())modelStatus.Text="Could not refresh models. Check the credentials/base URL and save again.";
                throw;
            } finally {_providerUpdate=null;_providerChange.Release();if(Current()){models.IsEnabled=true;provider.IsEnabled=key.IsEnabled=url.IsEnabled=model.IsEnabled=true;}}
            if(!Current())return;
            await LocalOp("set_reasoning_effort",("effort",reasoning.SelectedItem));
            await LocalOp("set_brave",("api_key",string.IsNullOrWhiteSpace(brave.Text)?null:brave.Text),("mode","context"));
            await LocalOp("set_web_search",("enabled",web.IsChecked==true));await LocalOp("memory_set",("enabled",memory.IsChecked==true));
            await LocalOp("set_subagent_defaults",("enabled",workers.IsChecked==true),("provider_id",workerProvider.SelectedItem),("model",string.IsNullOrWhiteSpace(workerModel.Text)?"inherit":workerModel.Text),("reasoning_effort",workerReasoning.SelectedItem));
            brave.Text="";_status.Text="Settings saved.";
        }));
    }
}
