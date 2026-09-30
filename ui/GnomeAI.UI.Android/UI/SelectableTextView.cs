using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Media;
using Avalonia.Media.TextFormatting;

namespace GnomeAI.Android.UI;

/// Drop-in replacement for the transcript <see cref="TextBlock"/> that behaves
/// like ordinary Android text: a tap puts a caret where the finger landed and a
/// drag extends the selection, with the selection painted inline. Text stays
/// non-editable, so no keyboard is involved.
public sealed class SelectableTextView : TextBlock
{
    public static readonly StyledProperty<int> SelectionStartProperty =
        AvaloniaProperty.Register<SelectableTextView,int>(nameof(SelectionStart));
    public static readonly StyledProperty<int> SelectionEndProperty =
        AvaloniaProperty.Register<SelectableTextView,int>(nameof(SelectionEnd));
    /// Caret index, or -1 when the caret is hidden.
    public static readonly StyledProperty<int> CaretIndexProperty =
        AvaloniaProperty.Register<SelectableTextView,int>(nameof(CaretIndex),-1);
    public static readonly StyledProperty<IBrush?> SelectionBrushProperty =
        AvaloniaProperty.Register<SelectableTextView,IBrush?>(nameof(SelectionBrush));
    /// Show the two drag handles around the current selection.
    public static readonly StyledProperty<bool> ShowHandlesProperty =
        AvaloniaProperty.Register<SelectableTextView,bool>(nameof(ShowHandles),true);

    /// Distance a finger may travel before the press is treated as a scroll
    /// rather than a tap.
    private const double TapSlop=10;
    private bool _pressed;
    private bool _scrolled;
    private bool _extendingSelection;
    private Point _pressPosition;
    private int _anchor;
    private bool _draggingHandle;
    private bool _draggingEndHandle;

    static SelectableTextView()
    {
        AffectsRender<SelectableTextView>(
            SelectionStartProperty,SelectionEndProperty,CaretIndexProperty,
            SelectionBrushProperty,ShowHandlesProperty);
        // A TextBlock is only hit-testable while it paints a background. Without
        // this the pointer passes straight through to the surrounding scroll
        // viewer, so taps never reach the text and no caret is ever placed.
        BackgroundProperty.OverrideDefaultValue<SelectableTextView>(Brushes.Transparent);
        FocusableProperty.OverrideDefaultValue<SelectableTextView>(true);
    }

    public int SelectionStart { get=>GetValue(SelectionStartProperty); set=>SetValue(SelectionStartProperty,value); }
    public int SelectionEnd { get=>GetValue(SelectionEndProperty); set=>SetValue(SelectionEndProperty,value); }
    public int CaretIndex { get=>GetValue(CaretIndexProperty); set=>SetValue(CaretIndexProperty,value); }
    public IBrush? SelectionBrush { get=>GetValue(SelectionBrushProperty); set=>SetValue(SelectionBrushProperty,value); }
    public bool ShowHandles { get=>GetValue(ShowHandlesProperty); set=>SetValue(ShowHandlesProperty,value); }
    public bool HasSelection => SelectionStart!=SelectionEnd;

    public event EventHandler? SelectionChanged;

    /// Handle geometry, shared with hit testing so a touch on a handle is still a
    /// touch on the text and keeps working after the widget is scrolled.
    private const double HandleRadius=9;
    /// Comfortably larger than the drawn handle so it is easy to grab, but small
    /// enough that a stray tap near a selection still scrolls the transcript.
    private const double HandleTouchRadius=20;

    public SelectableTextView()
    {
        // Long press is the Android convention for "select this word"; it is also
        // the only way to start a selection without dragging.
        Gestures.SetIsHoldingEnabled(this,true);
        Gestures.SetIsHoldWithMouseEnabled(this,true);
        AddHandler(Gestures.HoldingEvent,(_,e)=>{
            if(e.HoldingState!=HoldingState.Started)return;
            var text=Text??"";
            if(text.Length==0)return;
            var (start,finish)=WordRange(text,TextPositionAt(e.Position));
            if(start==finish)return;
            _anchor=start;SelectionStart=start;SelectionEnd=finish;CaretIndex=finish;
            // From here the gesture selects instead of scrolling. The press stays
            // live so the finger can keep dragging to extend the selection; the
            // flags are cleared when the pointer is released.
            _extendingSelection=true;
            Focus();
            e.Handled=true;
            SelectionChanged?.Invoke(this,EventArgs.Empty);
            InvalidateVisual();
        },RoutingStrategies.Bubble,true);
    }

    /// Visible text, so a selection can be copied without knowledge of Markdown.
    public string SelectedText {
        get {
            var text=Text??"";var start=Math.Clamp(Math.Min(SelectionStart,SelectionEnd),0,text.Length);
            var end=Math.Clamp(Math.Max(SelectionStart,SelectionEnd),start,text.Length);
            return end>start?text[start..end]:"";
        }
    }

    public void ClearSelection()
    {
        if(SelectionStart==0 && SelectionEnd==0 && CaretIndex<0)return;
        SelectionStart=0;SelectionEnd=0;CaretIndex=-1;
        SelectionChanged?.Invoke(this,EventArgs.Empty);
        InvalidateVisual();
    }

    protected override void OnPointerPressed(PointerPressedEventArgs e)
    {
        base.OnPointerPressed(e);
        if(!e.GetCurrentPoint(this).Properties.IsLeftButtonPressed)return;
        var position=e.GetPosition(this);
        _pressed=true;_scrolled=false;_draggingHandle=false;_draggingEndHandle=false;
        _pressPosition=position;
        // Multi-tap is checked before the handles: after the first tap the handles
        // sit right under the finger, and letting them win would turn the second
        // and third taps of a multi-tap into handle drags.
        if(e.ClickCount>=2) {
            // Double tap selects a word, triple tap the whole block — the standard
            // Android shortcuts — and both arm dragging to extend the selection.
            var text=Text??"";
            var index=TextPositionAt(position);
            if(e.ClickCount>=3){_anchor=0;SelectionStart=0;SelectionEnd=text.Length;CaretIndex=text.Length;}
            else {
                var (start,finish)=WordRange(text,index);
                _anchor=start;SelectionStart=start;SelectionEnd=finish;CaretIndex=finish;
            }
            _extendingSelection=true;
        } else if(HitHandle(position,out var endHandle)) {
            // A touch that lands on a handle keeps the opposite end of the selection
            // anchored, so dragging one handle never moves the other.
            _draggingHandle=true;_draggingEndHandle=endHandle;
            _anchor=endHandle?Math.Min(SelectionStart,SelectionEnd):Math.Max(SelectionStart,SelectionEnd);
            _extendingSelection=true;
        } else {
            // A plain press may still turn out to be a scroll, so it must not claim
            // the pointer or block gesture recognition, and it never extends an
            // existing selection. The caret is placed on release, once the gesture
            // is known to have been a tap.
            _extendingSelection=false;
        }
        if(_draggingHandle || _extendingSelection) {
            // Selection in progress: keep the transcript from scrolling under it.
            Focus();
            e.Pointer.Capture(this);
            e.PreventGestureRecognition();
            e.Handled=true;
            SelectionChanged?.Invoke(this,EventArgs.Empty);
            InvalidateVisual();
        }
    }

    protected override void OnPointerMoved(PointerEventArgs e)
    {
        base.OnPointerMoved(e);
        if(!_pressed || e.Pointer.Captured!=this)return;
        if(!e.GetCurrentPoint(this).Properties.IsLeftButtonPressed)return;
        var index=TextPositionAt(e.GetPosition(this));
        if(_draggingHandle) {
            if(_draggingEndHandle)SelectionEnd=index; else SelectionStart=index;
        } else if(_extendingSelection) {
            SelectionStart=Math.Min(_anchor,index);SelectionEnd=Math.Max(_anchor,index);
        } else {
            // Not selecting yet: past the slop this gesture belongs to the scroll
            // viewer, so the press must not place a caret when it is released.
            var delta=e.GetPosition(this)-_pressPosition;
            if(Math.Abs(delta.X)>TapSlop || Math.Abs(delta.Y)>TapSlop)_scrolled=true;
            return;
        }
        CaretIndex=index;
        e.PreventGestureRecognition();
        e.Handled=true;
        SelectionChanged?.Invoke(this,EventArgs.Empty);
        InvalidateVisual();
    }

    protected override void OnPointerReleased(PointerReleasedEventArgs e)
    {
        base.OnPointerReleased(e);
        if(!_pressed)return;
        var wasPressed=_pressed;_pressed=false;
        if(_draggingHandle || _extendingSelection) {
            _draggingHandle=false;_extendingSelection=false;
            e.Pointer.Capture(null);
            SelectionChanged?.Invoke(this,EventArgs.Empty);
            InvalidateVisual();
            return;
        }
        _draggingHandle=false;
        // A tap that never became a scroll places the caret, exactly like a native
        // text view. `ClearSelection` alone would leave the caret where it was.
        if(wasPressed && !_scrolled) {
            var text=Text??"";
            var index=TextPositionAt(e.GetPosition(this));
            _anchor=index;SelectionStart=index;SelectionEnd=index;
            CaretIndex=Math.Clamp(index,0,text.Length);
            SelectionChanged?.Invoke(this,EventArgs.Empty);
            InvalidateVisual();
        }
        _scrolled=false;
        _extendingSelection=false;
        e.Pointer.Capture(null);
        // A handle drag that collapses onto itself becomes a plain caret, which
        // matches the gesture the user is performing.
        if(HasSelection && Math.Min(SelectionStart,SelectionEnd)==Math.Max(SelectionStart,SelectionEnd))
            SelectionStart=SelectionEnd=CaretIndex;
    }

    protected override void OnPointerCaptureLost(PointerCaptureLostEventArgs e)
    {
        base.OnPointerCaptureLost(e);
        _pressed=false;_draggingHandle=false;_extendingSelection=false;
    }

    /// `TextLayout` measures against the rendered text, so padding must be
    /// removed before hit testing to translate a pointer into a character index.
    private int TextPositionAt(Point position)
    {
        var text=Text??"";
        if(text.Length==0)return 0;
        var layout=TextLayout;
        if(layout is null)return 0;
        var padding=Padding;
        var point=new Point(position.X-padding.Left,position.Y-padding.Top);
        var hit=layout.HitTestPoint(point);
        return Math.Clamp(hit.TextPosition,0,text.Length);
    }

    /// Word boundaries around `index`, used by a double tap. Whitespace between
    /// words is skipped so the selection never starts or ends on a space.
    private static (int Start,int End) WordRange(string text,int index)
    {
        var position=Math.Clamp(index,0,text.Length);
        var start=position;
        var end=position;
        while(start>0 && !char.IsWhiteSpace(text[start-1]))start--;
        while(end<text.Length && !char.IsWhiteSpace(text[end]))end++;
        if(start==end) {
            // The tap landed on whitespace: take the next word instead.
            while(end<text.Length && char.IsWhiteSpace(text[end]))end++;
            start=end;
            while(end<text.Length && !char.IsWhiteSpace(text[end]))end++;
        }
        return (start,end);
    }

    /// Rects of the two selection handles, in widget coordinates.
    private bool TryHandleRects(out Rect start,out Rect end)
    {
        start=default;end=default;
        var layout=TextLayout;
        if(layout is null || !HasSelection)return false;
        var first=Math.Min(SelectionStart,SelectionEnd);
        var last=Math.Max(SelectionStart,SelectionEnd);
        var padding=Padding;
        var startRect=layout.HitTestTextPosition(first);
        var endRect=layout.HitTestTextPosition(last);
        start=new Rect(padding.Left+startRect.X-HandleRadius,padding.Top+startRect.Bottom-HandleRadius,
            HandleRadius*2,HandleRadius*2);
        end=new Rect(padding.Left+endRect.X-HandleRadius,padding.Top+endRect.Bottom-HandleRadius,
            HandleRadius*2,HandleRadius*2);
        return true;
    }

    /// `endHandle` is true when the touch grabbed the handle after the selection.
    private bool HitHandle(Point position,out bool endHandle)
    {
        endHandle=false;
        if(!ShowHandles || !HasSelection)return false;
        if(!TryHandleRects(out var start,out var end))return false;
        var touch=new Rect(position.X-HandleTouchRadius,position.Y-HandleTouchRadius,
            HandleTouchRadius*2,HandleTouchRadius*2);
        // The end handle wins ties so the common "extend forwards" gesture is not
        // stolen by the start handle when the selection is a single word.
        if(touch.Intersects(end)){endHandle=true;return true;}
        return touch.Intersects(start);
    }

    protected override void RenderTextLayout(DrawingContext context,Point origin)
    {
        var layout=TextLayout;
        var brush=SelectionBrush;
        if(layout is not null && brush is not null && HasSelection) {
            var start=Math.Min(SelectionStart,SelectionEnd);
            var length=Math.Max(SelectionStart,SelectionEnd)-start;
            using(context.PushTransform(Matrix.CreateTranslation(origin)))
                foreach(var rect in layout.HitTestTextRange(start,length))
                    context.FillRectangle(brush,rect);
        }
        base.RenderTextLayout(context,origin);
        if(layout is null)return;
        using(context.PushTransform(Matrix.CreateTranslation(origin))) {
            if(ShowHandles && HasSelection && TryHandleRects(out var startHandle,out var endHandle)) {
                var fill=SelectionBrush??CaretBrush??Brushes.DodgerBlue;
                DrawHandle(context,fill,startHandle);
                DrawHandle(context,fill,endHandle);
            }
            if(CaretIndex<0 || HasSelection)return;
            var caret=layout.HitTestTextPosition(CaretIndex);
            var pen=new Pen(CaretBrush??Brushes.White,Math.Max(1,CaretThickness));
            context.DrawLine(pen,new Point(caret.X+0.5,caret.Y),new Point(caret.X+0.5,caret.Bottom));
        }
    }

    private static void DrawHandle(DrawingContext context,IBrush fill,Rect handle)
    {
        var center=handle.Center;
        context.DrawEllipse(fill,null,center,HandleRadius,HandleRadius);
    }

    public IBrush? CaretBrush { get; set; }
    public double CaretThickness { get; set; }=2;
}
