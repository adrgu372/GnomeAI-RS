using System.Text;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Styling;
using Avalonia.Threading;

using GnomeAI.Client;

namespace GnomeAI.UI;

/// <summary>
/// Lightweight native Markdown presenter for streamed assistant output.  It
/// deliberately keeps the parser small and deterministic while preserving the
/// structures the legacy desktop UI rendered: headings, lists, quotes, tables,
/// fenced code, rules and selectable paragraphs.
/// </summary>
public sealed class MarkdownView : UserControl
{
    public static readonly StyledProperty<string> MarkdownProperty =
        AvaloniaProperty.Register<MarkdownView, string>(nameof(Markdown), "");

    private bool IsDark => ActualThemeVariant == ThemeVariant.Dark;
    private IBrush MutedBrush => Brush.Parse(IsDark ? "#B5B5B5" : "#59636F");
    private IBrush CodeBrush => Brush.Parse(IsDark ? "#202020" : "#F3F5F7");
    private IBrush MarkdownBorderBrush => Brush.Parse(IsDark ? "#3C3C3C" : "#D8DDE5");
    private IBrush QuoteBrush => Brush.Parse(IsDark ? "#60CDFF" : "#4C8DCE");
    private IBrush TableHeaderBrush => Brush.Parse(IsDark ? "#343434" : "#EDF2F7");
    private IBrush TableCellBrush => Brush.Parse(IsDark ? "#2B2B2B" : "#FFFFFF");
    private readonly StackPanel _content = new() { Spacing = 7 };
    private readonly DispatcherTimer _renderTimer = new() { Interval = TimeSpan.FromMilliseconds(50) };
    private string _renderedMarkdown = "";
    private bool _renderedDark;
    private readonly StackPanel _root=new() {Spacing=8};
    private readonly Button _copyResponse=new() {Content="Copy response"};
    private readonly Button _selectText=new() {Content="Select text"};
    private readonly Button _copySelection=new() {Content="Copy selection",IsVisible=false};
    private readonly TextBox _selection=new() {IsReadOnly=true,AcceptsReturn=true,TextWrapping=TextWrapping.Wrap,MinLines=1,MaxLines=16,IsVisible=false};
    private bool _selectingText;
    public bool IsSelectingText=>_selectingText;
    public event EventHandler? SelectionModeChanged;

    public MarkdownView()
    {
        var actions=new WrapPanel {Orientation=Orientation.Horizontal};
        foreach(var button in new[]{_copyResponse,_selectText,_copySelection}) {
            button.Padding=new Thickness(10,6);button.MinHeight=OperatingSystem.IsAndroid()?44:30;
            button.Margin=new Thickness(0,0,6,0);actions.Children.Add(button);
        }
        ToolTip.SetTip(_copyResponse,"Copy the original response, preserving Markdown and line breaks");
        _copyResponse.Click+=async(_,_)=>await CopyExactAsync(_copyResponse,_selectingText?_selection.Text??"":Markdown??"");
        _copySelection.Click+=async(_,_)=> {
            var text=_selection.Text??"";
            var start=Math.Clamp(Math.Min(_selection.SelectionStart,_selection.SelectionEnd),0,text.Length);
            var end=Math.Clamp(Math.Max(_selection.SelectionStart,_selection.SelectionEnd),start,text.Length);
            if(end>start)await CopyExactAsync(_copySelection,text[start..end]);
            else {_copySelection.Content="Select some text first";}
        };
        _selectText.Click+=(_,_)=> {
            _selectingText=!_selectingText;
            _selectText.Content=_selectingText?"Back to response":"Select text";
            _copySelection.IsVisible=_selection.IsVisible=_selectingText;
            _content.IsVisible=!_selectingText;
            if(_selectingText) {
                // A fixed snapshot avoids losing touch selection as tokens arrive.
                _selection.Text=Markdown??"";_selection.SelectionStart=_selection.SelectionEnd=0;
                _selection.Focus();
            }else RebuildNow();
            SelectionModeChanged?.Invoke(this,EventArgs.Empty);
        };
        var copyMenu=new MenuItem {Header="Copy response"};
        var selectMenu=new MenuItem {Header="Select text"};
        var selectionMenu=new MenuItem {Header="Copy selection"};
        copyMenu.Click+=(_,_)=>_copyResponse.RaiseEvent(new Avalonia.Interactivity.RoutedEventArgs(Button.ClickEvent));
        selectMenu.Click+=(_,_)=>{_selectText.RaiseEvent(new Avalonia.Interactivity.RoutedEventArgs(Button.ClickEvent));selectMenu.Header=_selectText.Content;};
        selectionMenu.Click+=(_,_)=>_copySelection.RaiseEvent(new Avalonia.Interactivity.RoutedEventArgs(Button.ClickEvent));
        ContextMenu=new ContextMenu {ItemsSource=new[]{copyMenu,selectMenu,selectionMenu}};
        _root.Children.Add(_selection);_root.Children.Add(_content);Content=_root;
        _root.IsVisible=false;
        _renderTimer.Tick += (_, _) =>
        {
            _renderTimer.Stop();
            RebuildNow();
        };
        ActualThemeVariantChanged += (_, _) => QueueRebuild();
    }

    public string Markdown
    {
        get => GetValue(MarkdownProperty);
        set => SetValue(MarkdownProperty, value);
    }

    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == MarkdownProperty)
            QueueRebuild();
    }

    public void EndSelection() {
        if(!_selectingText)return;
        _selectingText=false;_selection.IsVisible=_copySelection.IsVisible=false;
        _content.IsVisible=true;_selectText.Content="Select text";_selection.Text="";
        RebuildNow();SelectionModeChanged?.Invoke(this,EventArgs.Empty);
    }

    private void QueueRebuild()
    {
        if (!_selectingText && !_renderTimer.IsEnabled) _renderTimer.Start();
    }

    private void RebuildNow()
    {
        if(_selectingText)return;
        var text = Markdown ?? "";
        _root.IsVisible=text.Length>0;
        var dark = IsDark;
        if (text == _renderedMarkdown && dark == _renderedDark) return;
        _renderedMarkdown = text;
        _renderedDark = dark;
        _content.Children.Clear();
        if (text.Length == 0)
            return;

        var lines = text.Replace("\r\n", "\n", StringComparison.Ordinal).Split('\n');
        var paragraph = new StringBuilder();
        var index = 0;

        void FlushParagraph()
        {
            if (paragraph.Length == 0) return;
            AddSelectable(paragraph.ToString().TrimEnd(), wrap: true);
            paragraph.Clear();
        }

        while (index < lines.Length)
        {
            var line = lines[index];
            var trimmed = line.Trim();
            if (TryOpenFence(trimmed,out var fenceMarker,out var fenceLength,out var language))
            {
                FlushParagraph();
                var code = new StringBuilder();
                index++;
                while (index < lines.Length && !IsClosingFence(lines[index],fenceMarker,fenceLength))
                {
                    code.Append(lines[index]);
                    if(index<lines.Length-1)code.Append('\n');
                    index++;
                }
                AddCode(language.Length == 0 ? "code" : language, code.ToString());
                if (index < lines.Length) index++;
                continue;
            }

            if (TryHeading(trimmed, out var level, out var heading))
            {
                FlushParagraph();
                _content.Children.Add(new SelectableTextBlock
                {
                    Text = CleanInline(heading),
                    FontSize = level switch { 1 => 23, 2 => 19, 3 => 16, _ => 14 },
                    FontWeight = level <= 2 ? FontWeight.SemiBold : FontWeight.Medium,
                    TextWrapping = TextWrapping.Wrap,
                    Margin = new Thickness(0, level == 1 ? 5 : 3, 0, 1),
                });
                index++;
                continue;
            }

            if (IsRule(trimmed))
            {
                FlushParagraph();
                _content.Children.Add(new Border { Height = 1, Background = MarkdownBorderBrush, Margin = new Thickness(0, 5) });
                index++;
                continue;
            }

            if (index + 1 < lines.Length && line.Contains('|') && IsTableDelimiter(lines[index + 1]))
            {
                FlushParagraph();
                var rows = new List<string[]> { TableCells(line) };
                index += 2;
                while (index < lines.Length && lines[index].Contains('|') && lines[index].Trim().Length > 0)
                {
                    rows.Add(TableCells(lines[index]));
                    index++;
                }
                AddTable(rows);
                continue;
            }

            if (trimmed.StartsWith('>'))
            {
                FlushParagraph();
                var quote = trimmed.TrimStart('>', ' ');
                var body = new SelectableTextBlock
                {
                    Text = CleanInline(quote),
                    TextWrapping = TextWrapping.Wrap,
                    Foreground = MutedBrush,
                };
                _content.Children.Add(new Border
                {
                    BorderBrush = QuoteBrush,
                    BorderThickness = new Thickness(3, 0, 0, 0),
                    Padding = new Thickness(10, 4),
                    Child = body,
                });
                index++;
                continue;
            }

            if (TryListItem(trimmed, out var marker, out var item))
            {
                FlushParagraph();
                var row = new Grid { ColumnDefinitions = new ColumnDefinitions("Auto,*"), ColumnSpacing = 8 };
                row.Children.Add(new TextBlock { Text = marker, Foreground = MutedBrush });
                var itemText = new SelectableTextBlock { Text = CleanInline(item), TextWrapping = TextWrapping.Wrap };
                Grid.SetColumn(itemText, 1);
                row.Children.Add(itemText);
                _content.Children.Add(row);
                index++;
                continue;
            }

            if (trimmed.Length == 0)
            {
                FlushParagraph();
                index++;
                continue;
            }

            if (paragraph.Length > 0) paragraph.Append('\n');
            paragraph.Append(line);
            index++;
        }

        FlushParagraph();
    }

    private async Task CopyExactAsync(Button button,string text) {
        var label=ReferenceEquals(button,_copySelection)?"Copy selection":ReferenceEquals(button,_copyResponse)?"Copy response":"Copy code";
        button.IsEnabled=false;
        try {
            var clipboard=TopLevel.GetTopLevel(this)?.Clipboard;
            if(clipboard is null){button.Content="Clipboard unavailable";return;}
            await clipboard.SetTextAsync(text);
            button.Content="Copied";
            await Task.Delay(1200);
            button.Content=label;
        }catch(Exception){button.Content="Copy failed · retry";}
        finally{button.IsEnabled=true;}
    }

    private void AddSelectable(string text, bool wrap)
    {
        var block = new SelectableTextBlock
        {
            Text = CleanInline(text),
            TextWrapping = wrap ? TextWrapping.Wrap : TextWrapping.NoWrap,
            FontSize = 14,
            HorizontalAlignment = HorizontalAlignment.Stretch,
        };
        _content.Children.Add(block);
    }

    private void AddCode(string language, string code)
    {
        var copy = new Button { Content = "Copy code", Padding = new Thickness(9, 5), MinHeight = OperatingSystem.IsAndroid()?44:30 };
        copy.Click += async (_, _) => await CopyExactAsync(copy,code);
        var header = new Grid { ColumnDefinitions = new ColumnDefinitions("*,Auto"), Margin = new Thickness(0, 0, 0, 5) };
        header.Children.Add(new TextBlock
        {
            Text = language,
            FontFamily = new FontFamily("monospace"),
            FontSize = 11,
            Foreground = MutedBrush,
            VerticalAlignment = VerticalAlignment.Center,
        });
        Grid.SetColumn(copy, 1);
        header.Children.Add(copy);

        var codeBlock = new SelectableTextBlock
        {
            Text = code,
            TextWrapping = TextWrapping.NoWrap,
            FontFamily = new FontFamily("monospace"),
            FontSize = 13,
        };
        var scroll = new ScrollViewer
        {
            HorizontalScrollBarVisibility = Avalonia.Controls.Primitives.ScrollBarVisibility.Auto,
            VerticalScrollBarVisibility = Avalonia.Controls.Primitives.ScrollBarVisibility.Disabled,
            IsScrollChainingEnabled = false,
            Content = codeBlock,
        };
        var stack = new StackPanel();
        stack.Children.Add(header);
        stack.Children.Add(scroll);
        _content.Children.Add(new Border
        {
            Background = CodeBrush,
            BorderBrush = MarkdownBorderBrush,
            BorderThickness = new Thickness(1),
            CornerRadius = new CornerRadius(6),
            Padding = new Thickness(10),
            Child = stack,
        });
    }

    private void AddTable(IReadOnlyList<string[]> rows)
    {
        if (rows.Count == 0) return;
        var columns = rows.Max(row => row.Length);
        var grid = new Grid();
        for (var column = 0; column < columns; column++)
            grid.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
        for (var row = 0; row < rows.Count; row++)
        {
            grid.RowDefinitions.Add(new RowDefinition(GridLength.Auto));
            for (var column = 0; column < columns; column++)
            {
                var cell = column < rows[row].Length ? CleanInline(rows[row][column]) : "";
                var border = new Border
                {
                    Background = row == 0 ? TableHeaderBrush : TableCellBrush,
                    BorderBrush = MarkdownBorderBrush,
                    BorderThickness = new Thickness(0.5),
                    Padding = new Thickness(8, 6),
                    Child = new SelectableTextBlock
                    {
                        Text = cell,
                        TextWrapping = TextWrapping.Wrap,
                        FontWeight = row == 0 ? FontWeight.SemiBold : FontWeight.Normal,
                    },
                };
                Grid.SetRow(border, row);
                Grid.SetColumn(border, column);
                grid.Children.Add(border);
            }
        }
        _content.Children.Add(new ScrollViewer
        {
            HorizontalScrollBarVisibility = Avalonia.Controls.Primitives.ScrollBarVisibility.Auto,
            Content = grid,
        });
    }

    private static bool TryOpenFence(string line,out char marker,out int length,out string language) {
        marker=line.Length>0?line[0]:' ';length=0;language="";
        if(marker is not ('`' or '~'))return false;
        while(length<line.Length && line[length]==marker)length++;
        if(length<3)return false;
        language=line[length..].Trim();return true;
    }
    private static bool IsClosingFence(string line,char marker,int minimum) {
        var text=line.Trim();var count=0;
        while(count<text.Length && text[count]==marker)count++;
        return count>=minimum && text[count..].Trim().Length==0;
    }

    private static bool TryHeading(string line, out int level, out string text)
    {
        level = 0;
        while (level < line.Length && level < 6 && line[level] == '#') level++;
        if (level == 0 || level >= line.Length || line[level] != ' ')
        {
            text = "";
            level = 0;
            return false;
        }
        text = line[(level + 1)..];
        return true;
    }

    private static bool TryListItem(string line, out string marker, out string text)
    {
        marker = "";
        text = "";
        if (line.StartsWith("- ", StringComparison.Ordinal) || line.StartsWith("* ", StringComparison.Ordinal))
        {
            marker = "•";
            text = line[2..];
            return true;
        }
        var dot = line.IndexOf(". ", StringComparison.Ordinal);
        if (dot is > 0 and < 5 && line[..dot].All(char.IsDigit))
        {
            marker = line[..(dot + 1)];
            text = line[(dot + 2)..];
            return true;
        }
        return false;
    }

    private static bool IsRule(string line) =>
        line is "---" or "***" or "___";

    private static bool IsTableDelimiter(string line)
    {
        var cells = TableCells(line);
        return cells.Length > 0 && cells.All(cell =>
        {
            var value = cell.Trim().Trim(':');
            return value.Length >= 3 && value.All(character => character == '-');
        });
    }

    private static string[] TableCells(string line) =>
        line.Trim().Trim('|').Split('|').Select(cell => cell.Trim()).ToArray();

    private static string CleanInline(string text)
    {
        // Keep inline code literal: __name__, ** and backslashes are data there.
        var output=new StringBuilder();var cursor=0;
        foreach(System.Text.RegularExpressions.Match match in System.Text.RegularExpressions.Regex.Matches(
            text,@"(?<!`)(`+)(?!`)(.*?)\1(?!`)",System.Text.RegularExpressions.RegexOptions.Singleline)) {
            output.Append(CleanEmphasis(text[cursor..match.Index]));
            output.Append(match.Groups[2].Value);cursor=match.Index+match.Length;
        }
        output.Append(CleanEmphasis(text[cursor..]));return output.ToString();
    }
    private static string CleanEmphasis(string text) {
        text=System.Text.RegularExpressions.Regex.Replace(text,@"(?<!\*)\*\*(?=\S)(.+?)(?<=\S)\*\*(?!\*)","$1");
        return System.Text.RegularExpressions.Regex.Replace(text,@"(?<![\w\\])__(?=\S)(.+?)(?<=\S)__(?!\w)","$1");
    }
}
