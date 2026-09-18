using System.Text;
using Avalonia.Controls;
using Avalonia.Media;
using Avalonia.Platform.Storage;

namespace GnomeAI.Android.UI;

public sealed partial class MainView
{
    private async Task ShowSkillsAsync()
    {
        ShowPanel(); SelectTab("Settings");
        var generation = _settingsGeneration;
        bool Current() => !_disposed && generation == _settingsGeneration;
        var session = Selected is null ? _session : "";
        _panel.Children.Add(new TextBlock { Text="Skills · This phone", FontSize=24, FontWeight=FontWeight.SemiBold });
        _panel.Children.Add(new TextBlock {
            Text="Import a standalone SKILL.md, inspect its instructions, then activate it for a local conversation. Tools remain subject to Android capabilities and the current execution mode.",
            TextWrapping=TextWrapping.Wrap
        });
        var feedback = new TextBlock { TextWrapping=TextWrapping.Wrap };
        var list = new StackPanel { Spacing=12 };
        async Task Reload()
        {
            var result = await _hub.RequestAsync(null,"skills_list",new { session_id=session });
            if (!Current()) return;
            list.Children.Clear();
            foreach (var item in result.GetProperty("skills").EnumerateArray()) {
                var name = item.GetProperty("name").GetString()!;
                var card = new StackPanel { Spacing=6 };
                card.Children.Add(new TextBlock { Text=name, FontWeight=FontWeight.SemiBold });
                card.Children.Add(new TextBlock { Text=item.GetProperty("description").GetString(), TextWrapping=TextWrapping.Wrap });
                var details = new TextBox { IsReadOnly=true, AcceptsReturn=true, TextWrapping=TextWrapping.Wrap, MaxHeight=240, IsVisible=false };
                card.Children.Add(Button("Inspect",async () => {
                    try {
                        var report = await _hub.RequestAsync(null,"skills_inspect",new {session_id=session,name});
                        if (!Current()) return;
                        details.Text=report.GetProperty("report").GetString(); details.IsVisible=true;
                    } catch(Exception error) { if(Current()) feedback.Text=error.Message; }
                }));
                var use = Button("Activate for this conversation", async () => {
                    try {
                        if(Selected is not null || _session!=session || session.Length==0)
                            throw new IOException("Open a conversation on This phone, then return to Skills.");
                        await _hub.RequestAsync(null,"skills_activate",new {session_id=session,name});
                        if(Current()) feedback.Text=$"Activated {name}. Send your next message in the conversation.";
                    } catch(Exception error) { if(Current()) feedback.Text=error.Message; }
                });
                use.IsEnabled=session.Length>0;
                card.Children.Add(use); card.Children.Add(details); list.Children.Add(card);
            }
            if(list.Children.Count==0) list.Children.Add(new TextBlock {Text="No installed skills yet."});
        }
        _panel.Children.Add(Button("Import SKILL.md…",async () => {
            string? folder=null;
            try {
                var storage=TopLevel.GetTopLevel(this)?.StorageProvider ?? throw new IOException("File picker is unavailable.");
                var files=await storage.OpenFilePickerAsync(new FilePickerOpenOptions {
                    Title="Import SKILL.md", AllowMultiple=false,
                    FileTypeFilter=new[] { new FilePickerFileType("Markdown skill") { Patterns=new[]{"*.md"}, MimeTypes=new[]{"text/markdown","text/plain"} }, FilePickerFileTypes.All }
                });
                if(files.Count==0 || !Current())return;
                using var file=files[0];
                using var bytes=new MemoryStream();
                await using(var input=await file.OpenReadAsync()) {
                    var buffer=new byte[8192]; int count;
                    while((count=await input.ReadAsync(buffer))>0) {
                        if(bytes.Length+count>256*1024)throw new IOException("SKILL.md exceeds 256 KiB.");
                        bytes.Write(buffer,0,count);
                    }
                }
                var markdown=new UTF8Encoding(false,true).GetString(bytes.ToArray());
                if(markdown.Length==0)throw new IOException("SKILL.md is empty.");
                if(!Current())return;
                folder=Path.Combine(_home,"store","skill-imports",Guid.NewGuid().ToString("N"));
                Directory.CreateDirectory(folder);
                var path=Path.Combine(folder,"SKILL.md");
                await File.WriteAllTextAsync(path,markdown.TrimStart('\uFEFF'),new UTF8Encoding(false));
                feedback.Text="Installing and validating skill…";
                var installed=await _hub.RequestAsync(null,"skills_install",new {session_id=session,source=path});
                if(!Current())return;
                feedback.Text="Installed "+installed.GetProperty("name").GetString()+". Inspect it before activation.";
                await Reload();
            } catch(Exception error) { if(Current())feedback.Text=error.Message; }
            finally {
                if(folder is not null)try {Directory.Delete(folder,true);} catch(IOException){} catch(UnauthorizedAccessException){}
            }
        }));
        _panel.Children.Add(Button("Refresh",Reload));
        _panel.Children.Add(Button("Back to settings",ShowSettingsAsync));
        _panel.Children.Add(feedback); _panel.Children.Add(list);
        try { await Reload(); } catch(Exception error) { if(Current())feedback.Text=error.Message; }
    }
}
