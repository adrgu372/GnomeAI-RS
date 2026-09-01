using System.Text;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Styling;
using Avalonia.Threading;

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

    public MarkdownView()
    {
        Content = _content;
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

    private void QueueRebuild()
    {
        if (!_renderTimer.IsEnabled) _renderTimer.Start();
    }

    private void RebuildNow()
    {
        var text = Markdown ?? "";
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
            if (trimmed.StartsWith("```", StringComparison.Ordinal))
            {
                FlushParagraph();
                var language = trimmed.Length > 3 ? trimmed[3..].Trim() : "code";
                var code = new StringBuilder();
                index++;
                while (index < lines.Length && !lines[index].TrimStart().StartsWith("```", StringComparison.Ordinal))
                {
                    if (code.Length > 0) code.Append('\n');
                    code.Append(lines[index]);
                    index++;
                }
                AddCode(language.Length == 0 ? "code" : language, code.ToString());
                if (index < lines.Length) index++;
                continue;
            }

            if (TryHeading(trimmed, out var level, out var heading))
            {
                FlushParagraph();
                _content.Children.Add(new TextBlock
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
                var body = new TextBlock
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
                var itemText = new TextBlock { Text = CleanInline(item), TextWrapping = TextWrapping.Wrap };
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
        var copy = new Button { Content = "Copy", Padding = new Thickness(9, 3), MinHeight = 26 };
        copy.Click += async (_, _) =>
        {
            var clipboard = TopLevel.GetTopLevel(this)?.Clipboard;
            if (clipboard is not null) await clipboard.SetTextAsync(code);
        };
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
                    Child = new TextBlock
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

    private static string CleanInline(string text) => text
        .Replace("**", "", StringComparison.Ordinal)
        .Replace("__", "", StringComparison.Ordinal)
        .Replace("`", "", StringComparison.Ordinal);
}
