using Avalonia;
using Avalonia.Controls;
using Avalonia.Threading;

namespace GnomeAI.UI;

/// <summary>
/// Protects the desktop window from the temporary fallback display geometry
/// some Linux compositors expose while a monitor wakes from standby.  A real
/// resize becomes the new baseline after it settles; a long event-loop gap
/// restores the last stable window before the fallback geometry can persist.
/// </summary>
public sealed class WindowGeometryRecovery : IDisposable
{
    private static readonly TimeSpan ResumeGap = TimeSpan.FromSeconds(2);
    private static readonly TimeSpan SettleTime = TimeSpan.FromMilliseconds(750);
    private static readonly TimeSpan RecoveryDelay = TimeSpan.FromMilliseconds(400);
    private static readonly TimeSpan RecoveryRetry = TimeSpan.FromMilliseconds(500);

    private readonly Window _window;
    private readonly DispatcherTimer _timer = new() { Interval = TimeSpan.FromSeconds(1) };
    private DateTimeOffset _lastObservation = DateTimeOffset.UtcNow;
    private DateTimeOffset _candidateSince = DateTimeOffset.UtcNow;
    private DateTimeOffset? _nextRecovery;
    private Geometry? _stable;
    private Geometry? _candidate;
    private int _recoveryAttempts;
    private bool _applying;

    public WindowGeometryRecovery(Window window)
    {
        _window = window;
        _timer.Tick += Timer_Tick;
        _window.Opened += Window_Opened;
        _window.Closed += Window_Closed;
    }

    private void Window_Opened(object? sender, EventArgs e)
    {
        var current = Capture();
        if (current is not null) _stable = _candidate = current;
        _lastObservation = DateTimeOffset.UtcNow;
        _timer.Start();
    }

    private void Window_Closed(object? sender, EventArgs e) => Dispose();

    private void Timer_Tick(object? sender, EventArgs e)
    {
        var now = DateTimeOffset.UtcNow;
        var gap = now - _lastObservation;
        _lastObservation = now;
        var current = Capture();
        if (current is null) return;

        if (gap >= ResumeGap && _stable is not null)
        {
            _recoveryAttempts = 3;
            _nextRecovery = now + RecoveryDelay;
        }

        if (_nextRecovery is not null && now >= _nextRecovery && _stable is not null)
        {
            if (!ApproximatelyEqual(current, _stable)) Apply(_stable);
            _recoveryAttempts--;
            _nextRecovery = _recoveryAttempts > 0 ? now + RecoveryRetry : null;
            return;
        }

        if (_applying || current.State == WindowState.Minimized) return;
        if (_candidate is null || !ApproximatelyEqual(current, _candidate))
        {
            _candidate = current;
            _candidateSince = now;
            return;
        }
        if (now - _candidateSince >= SettleTime) _stable = current;
    }

    private Geometry? Capture()
    {
        if (!_window.IsVisible || _window.ClientSize.Width < 100 || _window.ClientSize.Height < 100) return null;
        // Screen enumeration may synchronously query X11/Wayland. Sampling it
        // four times per second while the monitor sleeps can stall the UI
        // thread, and the recovery decision only needs the window geometry.
        return new Geometry(_window.ClientSize, _window.Position, _window.WindowState);
    }

    private void Apply(Geometry geometry)
    {
        _applying = true;
        var targetState = geometry.State;
        _window.WindowState = WindowState.Normal;
        _window.Width = geometry.ClientSize.Width;
        _window.Height = geometry.ClientSize.Height;
        _window.Position = geometry.Position;
        DispatcherTimer.RunOnce(() =>
        {
            if (targetState is WindowState.Maximized or WindowState.FullScreen)
                _window.WindowState = targetState;
            _applying = false;
        }, TimeSpan.FromMilliseconds(120));
    }

    private static bool ApproximatelyEqual(Geometry left, Geometry right)
    {
        const double sizeTolerance = 5;
        const int positionTolerance = 20;
        return Math.Abs(left.ClientSize.Width - right.ClientSize.Width) <= sizeTolerance
            && Math.Abs(left.ClientSize.Height - right.ClientSize.Height) <= sizeTolerance
            && Math.Abs(left.Position.X - right.Position.X) <= positionTolerance
            && Math.Abs(left.Position.Y - right.Position.Y) <= positionTolerance
            && left.State == right.State;
    }

    public void Dispose()
    {
        _timer.Stop();
        _timer.Tick -= Timer_Tick;
        _window.Opened -= Window_Opened;
        _window.Closed -= Window_Closed;
    }

    private sealed record Geometry(Size ClientSize, PixelPoint Position, WindowState State);
}
