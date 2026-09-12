using Button = Avalonia.Controls.Button;
using Orientation = Avalonia.Layout.Orientation;
using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;
using VectorPath = Avalonia.Controls.Shapes.Path;

using GnomeAI.Client;

namespace GnomeAI.Android.UI;

public sealed partial class MainView
{
    private bool Mobile=>OperatingSystem.IsAndroid();
    private Border? _composeSurface;
    private Grid? _bottomNavigation;
    private void ApplyWindowInsets(Thickness padding,bool keyboard)=>Avalonia.Threading.Dispatcher.UIThread.Post(()=> {
        if(_disposed)return;
        Padding=padding;
        if(_bottomNavigation is not null)_bottomNavigation.IsVisible=!keyboard;
    });
    private readonly Dictionary<string,Button> _tabs=[];
    private static IBrush Ink(string hex)=>Brush.Parse(hex);
    private static Control Symbol(string name,IBrush? color=null)
    {
        var geometry=name switch {
            "camera"=>"M4 7H8L10 4H14L16 7H20V20H4Z M16 13A4 4 0 1 1 8 13A4 4 0 1 1 16 13Z",
            "files"=>"M21.44 11.05L12.25 20.24A6 6 0 0 1 3.76 11.75L12.95 2.56A4 4 0 0 1 18.61 8.22L9.41 17.41A2 2 0 0 1 6.58 14.58L15.07 6.1",
            "chat"=>"M4 4H20V17H9L4 21Z M8 9H16 M8 13H13",
            "chats"=>"M6 3H21V15H17 M3 7H17V19H8L3 22Z",
            "devices"=>"M2 4H15V14H2Z M5 19H12 M9 14V19 M17 8H23V21H17Z M19 18H21",
            "settings"=>"M4 6H20 M4 12H20 M4 18H20 M8 3V9 M16 9V15 M10 15V21",
            "plus"=>"M12 4V20 M4 12H20",
            "close"=>"M6 6L18 18 M18 6L6 18",
            "send"=>"M12 20V4 M5 11L12 4L19 11",
            "stop"=>"M6 6H18V18H6Z",
            "more"=>"M5 12H5.1 M12 12H12.1 M19 12H19.1",
            _=>"M4 12L10 18L20 6"
        };
        return new VectorPath {Data=Geometry.Parse(geometry),Stroke=color??Ink("#AEC5BA"),StrokeThickness=1.8,
            StrokeLineCap=PenLineCap.Round,Width=22,Height=22,Stretch=Stretch.Uniform};
    }
    private Button IconButton(string icon,string label,Func<Task> action)
    {
        var button=Button(label,action);button.Content=Symbol(icon);button.Classes.Add("icon");
        AutomationProperties.SetName(button,label);ToolTip.SetTip(button,label);return button;
    }
    private Grid BuildMobileLayout()
    {
        var root=new Grid {RowDefinitions=new RowDefinitions("Auto,Auto,*,Auto,Auto"),Background=Ink("#10191E")};
        var header=new Grid {ColumnDefinitions=new ColumnDefinitions("Auto,*,Auto,Auto"),Margin=new Thickness(20,12,12,4)};
        header.Children.Add(new Border {Width=38,Height=38,CornerRadius=new CornerRadius(13),Background=Ink("#A5E3C5"),
            Child=new TextBlock {Text="G",FontSize=23,FontWeight=FontWeight.Bold,Foreground=Ink("#123226"),HorizontalAlignment=HorizontalAlignment.Center,VerticalAlignment=VerticalAlignment.Center}});
        var brand=new TextBlock {Text="GnomeAI",FontSize=23,FontWeight=FontWeight.SemiBold,Margin=new Thickness(10,0),VerticalAlignment=VerticalAlignment.Center};
        Grid.SetColumn(brand,1);header.Children.Add(brand);
        var add=IconButton("plus","New conversation",NewAsync);Grid.SetColumn(add,2);header.Children.Add(add);
        var more=IconButton("more","Conversation options",ShowConversationMenuAsync);Grid.SetColumn(more,3);header.Children.Add(more);root.Children.Add(header);
        var context=new Grid {ColumnDefinitions=new ColumnDefinitions("Auto,*"),Margin=new Thickness(20,0,20,10),ColumnSpacing=12};
        _source.MinWidth=120;_source.MaxWidth=190;_source.FontSize=12;context.Children.Add(_source);
        _status.FontSize=11;_status.Foreground=Ink("#94ABA0");_status.TextWrapping=TextWrapping.NoWrap;_status.TextTrimming=TextTrimming.CharacterEllipsis;
        _status.VerticalAlignment=VerticalAlignment.Center;Grid.SetColumn(_status,1);context.Children.Add(_status);Grid.SetRow(context,1);root.Children.Add(context);
        _messages.Margin=new Thickness(20,16,20,28);_messages.Spacing=20;
        _panel.Margin=new Thickness(20,16,20,24);_panel.Spacing=14;
        _scroll.Content=_messages;Grid.SetRow(_scroll,2);root.Children.Add(_scroll);
        _panelScroll.Content=_panel;Grid.SetRow(_panelScroll,2);root.Children.Add(_panelScroll);
        var compose=new StackPanel {Spacing=6};compose.Children.Add(_attachmentCard);
        _composer.Classes.Add("composer");
        // TextBox measures wrapped lines, including pasted text, using its own
        // font metrics. Only its internal viewport is capped after six lines.
        _composer.Height=double.NaN;_composer.MinHeight=0;_composer.MaxHeight=double.PositiveInfinity;
        _composer.MinLines=1;_composer.MaxLines=6;_composer.LineHeight=22;
        ScrollViewer.SetVerticalScrollBarVisibility(_composer,Avalonia.Controls.Primitives.ScrollBarVisibility.Auto);
        ScrollViewer.SetHorizontalScrollBarVisibility(_composer,Avalonia.Controls.Primitives.ScrollBarVisibility.Disabled);
        _composer.Watermark="Ask anything, or add a photo…";compose.Children.Add(_composer);
        var actions=new Grid {ColumnDefinitions=new ColumnDefinitions("Auto,Auto,*,Auto")};
        var camera=IconButton("camera","Take a photo",CaptureAsync);camera.IsVisible=_capturePhoto is not null;actions.Children.Add(camera);
        var files=IconButton("files","Files · attach a file",AttachAsync);Grid.SetColumn(files,1);actions.Children.Add(files);
        _send.Classes.Add("icon");_send.Classes.Add("send");_send.Content=Symbol("send",Ink("#123226"));
        AutomationProperties.SetName(_send,"Send message");_send.Click+=async(_,_)=>await Guard(SendAsync);Grid.SetColumn(_send,3);actions.Children.Add(_send);compose.Children.Add(actions);
        _composeSurface=new Border {Child=compose,Padding=new Thickness(10),Margin=new Thickness(14,4,14,10),CornerRadius=new CornerRadius(24),
            Background=Ink("#1B282E"),BorderThickness=new Thickness(1),BorderBrush=Ink("#31433B")};
        Grid.SetRow(_composeSurface,3);root.Children.Add(_composeSurface);
        var navigation=_bottomNavigation=new Grid {ColumnDefinitions=new ColumnDefinitions("*,*,*,*"),ColumnSpacing=4,Margin=new Thickness(12,0,12,10)};
        AddTab(navigation,0,"Chat","chat",async()=>{ShowTranscript();if(_session.Length>0)await RefreshCurrentAsync();else{_messages.Children.Clear();_messages.Children.Add(Welcome());}});
        AddTab(navigation,1,"Chats","chats",ShowChatsAsync);
        AddTab(navigation,2,"Devices","devices",()=>{ShowDevices();return Task.CompletedTask;});
        AddTab(navigation,3,"Settings","settings",ShowSettingsAsync);
        Grid.SetRow(navigation,4);root.Children.Add(navigation);return root;
    }
    private void AddTab(Grid host,int column,string label,string icon,Func<Task> action)
    {
        var content=new StackPanel {Spacing=4,HorizontalAlignment=HorizontalAlignment.Center};
        content.Children.Add(Symbol(icon));content.Children.Add(new TextBlock {Text=label,FontSize=10,HorizontalAlignment=HorizontalAlignment.Center});
        var button=Button(label,action);button.Content=content;button.Classes.Add("nav");AutomationProperties.SetName(button,label);
        _tabs[label]=button;Grid.SetColumn(button,column);host.Children.Add(button);
    }
    private void SelectTab(string name) {foreach(var pair in _tabs){pair.Value.Classes.Set("active",pair.Key==name);}}
    private Control Welcome()
    {
        var panel=new StackPanel {Spacing=16,Margin=new Thickness(4,24,4,16)};
        panel.Children.Add(new TextBlock {Text="A little help.\nA bigger idea.",FontSize=32,LineHeight=38,FontWeight=FontWeight.SemiBold,TextWrapping=TextWrapping.Wrap});
        panel.Children.Add(new TextBlock {Text="Think it through, build something, or show me what you see.",FontSize=15,Foreground=Ink("#9FB4A9"),TextWrapping=TextWrapping.Wrap,Margin=new Thickness(0,0,0,8)});
        if(_capturePhoto is not null)panel.Children.Add(PromptCard("camera","Explore a photo","Point your camera at something",CaptureAsync));
        panel.Children.Add(PromptCard("chat","Think it through","A question, a plan, a fresh perspective",()=>{ShowTranscript();_composer.Focus();return Task.CompletedTask;}));
        panel.Children.Add(PromptCard("devices","Continue anywhere","Pick up a conversation from your PC",()=>{ShowDevices();return Task.CompletedTask;}));
        return panel;
    }
    private Button PromptCard(string icon,string title,string subtitle,Func<Task> action)
    {
        var row=new Grid {ColumnDefinitions=new ColumnDefinitions("Auto,*"),ColumnSpacing=14};row.Children.Add(Symbol(icon));
        var label=new StackPanel {Spacing=4};label.Children.Add(new TextBlock {Text=title,FontWeight=FontWeight.SemiBold});
        label.Children.Add(new TextBlock {Text=subtitle,FontSize=12,Foreground=Ink("#9FB4A9"),TextWrapping=TextWrapping.Wrap});Grid.SetColumn(label,1);row.Children.Add(label);
        var button=Button(title,action);button.Content=row;button.HorizontalContentAlignment=HorizontalAlignment.Stretch;return button;
    }
    private static Control SettingField(string label,Control control) {
        var field=new StackPanel {Spacing=6};field.Children.Add(new TextBlock {Text=label,FontSize=12,Foreground=Ink("#9FB4A9")});field.Children.Add(control);return field;
    }
    private void AddSettingsGroup(string title,params Control[] controls) {
        var body=new StackPanel {Spacing=12};body.Children.Add(new TextBlock {Text=title,FontSize=17,FontWeight=FontWeight.SemiBold,Margin=new Thickness(0,0,0,4)});
        foreach(var control in controls)body.Children.Add(control);
        _panel.Children.Add(new Border {Child=body,Background=Ink("#1B282E"),CornerRadius=new CornerRadius(20),Padding=new Thickness(16)});
    }
    private void UpdateSendAppearance()
    {
        if(Mobile){_send.Content=Symbol(_running?"stop":"send",Ink("#123226"));AutomationProperties.SetName(_send,_running?"Stop response":"Send message");}
        else _send.Content=_running?"Stop":"Send ↑";
    }
}
