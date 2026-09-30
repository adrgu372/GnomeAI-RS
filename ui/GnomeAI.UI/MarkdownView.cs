using System.Text;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Styling;
using Avalonia.Threading;
using Avalonia.VisualTree;

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
    private readonly MenuFlyout _blockFlyout=new();
    /// True while the user is holding a selection, so callers can defer
    /// transcript refreshes instead of wiping the highlighted text.
    public bool IsSelectingText=>Blocks().Any(block=>block.SelectionStart!=block.SelectionEnd);
    public event EventHandler? SelectionModeChanged;

    public MarkdownView()
    {
        _copyResponse.Padding=new Thickness(10,6);_copyResponse.MinHeight=OperatingSystem.IsAndroid()?44:30;
        _copyResponse.Margin=new Thickness(0,0,6,0);
        ToolTip.SetTip(_copyResponse,"Copy the original response, preserving Markdown and line breaks");
        _copyResponse.Click+=async(_,_)=>await CopyExactAsync(_copyResponse,FullTextForCopy());
        // The whole reply is the primary unit of copying; per-block selection is
        // the secondary one. Both must be reachable from one visible row.
        var copyMenu=new MenuItem {Header="Copy whole response"};
        var selectedMenu=new MenuItem {Header="Copy selected text"};
        var selectMenu=new MenuItem {Header="Select all text"};
        copyMenu.Click+=(_,_)=>_=CopyExactAsync(_copyResponse,FullTextForCopy());
        selectedMenu.Click+=(_,_)=>CopySelectedText();
        selectMenu.Click+=(_,_)=>SelectAllText();
        _blockFlyout.ItemsSource=new[] {selectedMenu,copyMenu,selectMenu};
        var actions=new StackPanel {Orientation=Orientation.Horizontal};
        actions.Children.Add(_copyResponse);
        _root.Children.Add(actions);
        _root.Children.Add(_content);Content=_root;
        _root.IsVisible=false;
        // Response-level shortcuts. Without these, Ctrl+A and Ctrl+C are handled by
        // whichever block owns focus, so a multi-paragraph reply is copied as one line.
        AddHandler(KeyDownEvent,OnResponseKeyDown,RoutingStrategies.Tunnel);
        // Selection is owned by this view, not by a single block. Each paragraph is
        // its own SelectableTextBlock, so without a view-level gesture a drag stops
        // at the block where it started and a reply cannot be selected across
        // paragraphs the way every other desktop application allows.
        AddHandler(PointerPressedEvent,OnResponsePointerPressed,RoutingStrategies.Tunnel,true);
        AddHandler(PointerMovedEvent,OnResponsePointerMoved,RoutingStrategies.Tunnel,true);
        AddHandler(PointerReleasedEvent,OnResponsePointerReleased,RoutingStrategies.Tunnel,true);
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
        if(!IsSelectingText)return;
        ClearBlockSelection();
        RebuildNow();SelectionModeChanged?.Invoke(this,EventArgs.Empty);
    }

    /// The blocks that currently hold a text selection, in visual order.
    private IEnumerable<SelectableTextBlock> Blocks()=>
        _content.GetVisualDescendants().OfType<SelectableTextBlock>();

    /// A drag that crosses paragraph boundaries is owned by this view. A block only
    /// sees the pointer while it is over that block, so a selection spanning
    /// several paragraphs has to be assembled here.
    private bool _draggingAcrossBlocks;
    private SelectableTextBlock? _dragAnchorBlock;
    private int _dragAnchorIndex;
    /// Block list and origins captured when the gesture starts. Rows are nested
    /// (list rows, table cells), so locating the pointer by walking the tree on
    /// every move would repeat the same work dozens of times per drag.
    private List<SelectableTextBlock>? _gestureBlocks;
    private List<Point>? _gestureOrigins;

    /// Visual order of the selectable blocks, flattened once per gesture. The
    /// hierarchy nests blocks (list rows, table cells), so depth-first order is the
    /// reading order a user sees.
    private List<SelectableTextBlock> OrderedBlocks()=>
        _content.GetVisualDescendants().OfType<SelectableTextBlock>().ToList();

    /// Selects from the anchor to `index` inside `block`, spanning every paragraph
    /// in between. Text runs are disjoint by construction, so the copied result
    /// reads top to bottom exactly like the reply.
    private void ApplyCrossBlockSelection(SelectableTextBlock block,int index)
    {
        var ordered=_gestureBlocks ?? OrderedBlocks();
        var anchor=ordered.IndexOf(_dragAnchorBlock!);
        var current=ordered.IndexOf(block);
        if(anchor<0 || current<0) {
            // The transcript was rebuilt under the drag; fall back to the block.
            _draggingAcrossBlocks=false;
            return;
        }
        if(anchor==current) {
            block.SelectionStart=Math.Min(_dragAnchorIndex,index);
            block.SelectionEnd=Math.Max(_dragAnchorIndex,index);
            return;
        }
        var first=Math.Min(anchor,current);
        var last=Math.Max(anchor,current);
        for(var position=first;position<=last;position++) {
            var target=ordered[position];
            var length=(target.Text??"").Length;
            if(position==first && position==anchor) {
                target.SelectionStart=Math.Min(_dragAnchorIndex,length);
                target.SelectionEnd=length;
            } else if(position==first) {
                // Dragging upwards: the pointer marks the top edge, so the block
                // under it contributes from the pointer to its own end.
                target.SelectionStart=Math.Min(index,length);
                target.SelectionEnd=length;
            } else if(position==last && position==anchor) {
                target.SelectionStart=0;
                target.SelectionEnd=Math.Min(_dragAnchorIndex,length);
            } else if(position==last) {
                // Dragging downwards: the pointer marks the bottom edge, so the
                // block under it contributes from its start to the pointer.
                target.SelectionStart=0;
                target.SelectionEnd=Math.Min(index,length);
            } else {
                target.SelectionStart=0;
                target.SelectionEnd=length;
            }
        }
    }

    private void OnResponsePointerPressed(object? sender,PointerPressedEventArgs e)
    {
        _draggingAcrossBlocks=false;
        _gestureBlocks=null;_gestureOrigins=null;
        if(!e.GetCurrentPoint(this).Properties.IsLeftButtonPressed)return;
        if(e.Source is not Visual source)return;
        var block=source as SelectableTextBlock ?? source.FindAncestorOfType<SelectableTextBlock>();
        if(block is null || !_content.GetVisualDescendants().Contains(block))return;
        // Arm the gesture. A plain click is left to the block itself; only once the
        // pointer leaves the block does this view take over the selection.
        _dragAnchorBlock=block;
        _dragAnchorIndex=TextIndexAt(block,e.GetPosition(block));
        _gestureBlocks=OrderedBlocks();
        _gestureOrigins=_gestureBlocks.Select(item=>item.TranslatePoint(new Point(0,0),this) ?? new Point(double.NaN,double.NaN)).ToList();
    }

    private void OnResponsePointerMoved(object? sender,PointerEventArgs e)
    {
        if(_dragAnchorBlock is null)return;
        if(!e.GetCurrentPoint(this).Properties.IsLeftButtonPressed)return;
        var hit=PointerOver(e);
        if(hit is null)return;
        var (block,index)=hit.Value;
        if(!_draggingAcrossBlocks) {
            // Still inside the anchor block: the block's own selection is correct
            // and already visible, so nothing to do until the drag leaves it.
            if(ReferenceEquals(block,_dragAnchorBlock))return;
            _draggingAcrossBlocks=true;
        }
        ApplyCrossBlockSelection(block,index);
        // The block would otherwise recompute its own selection from the pointer
        // clamped to its own bounds, which fights the range assembled here — the
        // upward case in particular would collapse back to an empty selection.
        e.Handled=true;
        SelectionModeChanged?.Invoke(this,EventArgs.Empty);
    }

    private void OnResponsePointerReleased(object? sender,PointerReleasedEventArgs e)
    {
        if(_draggingAcrossBlocks)SelectionModeChanged?.Invoke(this,EventArgs.Empty);
        _draggingAcrossBlocks=false;
        _dragAnchorBlock=null;
        _gestureBlocks=null;_gestureOrigins=null;
    }

    /// The selectable block under `e`, with the character index inside it. Blocks
    /// are laid out in a scroll viewer, so the point is resolved against each
    /// block's own bounds rather than assumed to be inside one.
    private (SelectableTextBlock Block,int Index)? PointerOver(PointerEventArgs e)
    {
        var ordered=_gestureBlocks ?? OrderedBlocks();
        var origins=_gestureOrigins;
        // The gesture's captured layout is valid only for this drag; a rebuild
        // during it invalidates both lists and the block's own handling resumes.
        if(_gestureBlocks is not null && origins is null)return null;
        for(var i=0;i<ordered.Count;i++) {
            var candidate=ordered[i];
            if(candidate.Bounds.Width<=0 || candidate.Bounds.Height<=0)continue;
            var origin=origins is not null?origins[i]:candidate.TranslatePoint(new Point(0,0),this) ?? new Point(double.NaN,double.NaN);
            if(double.IsNaN(origin.X) || double.IsNaN(origin.Y))continue;
            var local=e.GetPosition(this)-origin;
            if(local.X<-2 || local.Y<-2)continue;
            if(local.X>candidate.Bounds.Width+2 || local.Y>candidate.Bounds.Height+2)continue;
            return (candidate,TextIndexAt(candidate,local));
        }
        return null;
    }

    /// Character index for a point in a block's own coordinates. `TextLayout`
    /// excludes padding, so padding is removed before hit testing.
    private static int TextIndexAt(SelectableTextBlock block,Point position)
    {
        var text=block.Text??"";
        if(text.Length==0)return 0;
        var layout=block.TextLayout;
        if(layout is null)return 0;
        var padding=block.Padding;
        var hit=layout.HitTestPoint(new Point(position.X-padding.Left,position.Y-padding.Top));
        return Math.Clamp(hit.TextPosition,0,text.Length);
    }
    private void ClearBlockSelection()
    {
        foreach(var block in Blocks())block.ClearSelection();
    }
    private void SelectAllText()
    {
        var blocks=Blocks().ToArray();
        // Focus first, then select. Focusing a block clears the selection of the
        // block that loses focus, so selecting before focusing would drop everything
        // except the newly focused block.
        blocks.FirstOrDefault()?.Focus();
        foreach(var block in blocks)block.SelectAll();
        SelectionModeChanged?.Invoke(this,EventArgs.Empty);
    }
    private string SelectedBlockText()=>
        string.Join("\n",Blocks().Select(block=>block.SelectedText).Where(text=>!string.IsNullOrEmpty(text)));
    /// Response-level keyboard handling. Left to the blocks themselves, Ctrl+A
    /// selects only the focused block and Ctrl+C copies only that fragment, which
    /// silently truncates a multi-paragraph reply to a single line.
    private void OnResponseKeyDown(object? sender,KeyEventArgs e)
    {
        if(!e.KeyModifiers.HasFlag(KeyModifiers.Control))return;
        if(e.Key==Key.A){e.Handled=true;SelectAllText();return;}
        if(e.Key!=Key.C)return;
        e.Handled=true;
        var selected=SelectedBlockText();
        _=CopyExactAsync(_copyResponse,selected.Length>0?selected:FullTextForCopy());
    }
    private async void CopySelectedText()
    {
        var text=SelectedBlockText();
        if(text.Length==0){_copyResponse.Content="Select some text first";return;}
        await CopyExactAsync(_copyResponse,text);
    }
    /// Whole-reply copy: the original Markdown, never a partial block selection.
    private string FullTextForCopy()=>Markdown??"";
    private void BlockSelectionChanged()=>SelectionModeChanged?.Invoke(this,EventArgs.Empty);

    /// A block keeps the response-level menu for the whole reply, and reports
    /// its own selection so snapshot refreshes do not clear live highlighting.
    private void TrackSelection(SelectableTextBlock block)
    {
        // Without this the theme's own one-item flyout would replace the reply
        // menu and offer only the block's current selection.
        block.ContextFlyout=_blockFlyout;
        block.PropertyChanged+=(_,change)=>{
            if(change.Property!=SelectableTextBlock.SelectionStartProperty &&
               change.Property!=SelectableTextBlock.SelectionEndProperty)return;
            BlockSelectionChanged();
        };
        block.GotFocus+=(_,_)=>BlockSelectionChanged();
    }

    private void QueueRebuild()
    {
        if (!IsSelectingText && !_renderTimer.IsEnabled) _renderTimer.Start();
    }

    private void RebuildNow()
    {
        // Rebinding text while the user is selecting would drop the selection.
        if(IsSelectingText)return;
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
                var headingBlock = new SelectableTextBlock
                {
                    Text = CleanInline(heading),
                    FontSize = level switch { 1 => 23, 2 => 19, 3 => 16, _ => 14 },
                    FontWeight = level <= 2 ? FontWeight.SemiBold : FontWeight.Medium,
                    TextWrapping = TextWrapping.Wrap,
                    Margin = new Thickness(0, level == 1 ? 5 : 3, 0, 1),
                };
                TrackSelection(headingBlock);
                _content.Children.Add(headingBlock);
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
                TrackSelection(body);
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
                TrackSelection(itemText);
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
        var label=ReferenceEquals(button,_copyResponse)?"Copy response":"Copy code";
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
        TrackSelection(block);
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
        TrackSelection(codeBlock);
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
                var cellText = new SelectableTextBlock
                {
                    Text = cell,
                    TextWrapping = TextWrapping.Wrap,
                    FontWeight = row == 0 ? FontWeight.SemiBold : FontWeight.Normal,
                };
                TrackSelection(cellText);
                var border = new Border
                {
                    Background = row == 0 ? TableHeaderBrush : TableCellBrush,
                    BorderBrush = MarkdownBorderBrush,
                    BorderThickness = new Thickness(0.5),
                    Padding = new Thickness(8, 6),
                    Child = cellText,
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
