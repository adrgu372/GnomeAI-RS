using Avalonia.Interactivity;
using Avalonia.Threading;

namespace GnomeAI.UI;

public sealed partial class MainWindow
{
    private readonly ComposerDraftStore _draftStore = new();
    private readonly HashSet<string> _dirtyDraftSessions = [];
    private readonly DispatcherTimer _draftSaveTimer = new() { Interval = TimeSpan.FromMilliseconds(750) };
    private bool _restoringComposer;
    private bool _dispatching;
    private bool _coreConnected;
    private bool _closed;
    private bool _draftSaveErrorShown;

    private void CaptureComposerDraft()
    {
        if (_restoringComposer || _sessionTransitioning || _currentSessionId.Length == 0) return;
        var runtime = RuntimeFor(_currentSessionId);
        var draft = new ComposerDraft(Composer.Text ?? "", _attachments.FirstOrDefault(), Composer.CaretIndex);
        if (runtime.Draft == draft) return;
        runtime.Draft = draft;
        MarkDraftChanged(_currentSessionId);
    }

    private void MarkDraftChanged(string sessionId)
    {
        if (sessionId.Length == 0) return;
        _dirtyDraftSessions.Add(RuntimeKey(sessionId));
        _draftSaveTimer.Stop();
        if (!_closed) _draftSaveTimer.Start();
    }

    private void SaveComposerDrafts()
    {
        _draftSaveTimer.Stop();
        foreach (var sessionId in _dirtyDraftSessions.ToArray())
        {
            var runtime = _sessionRuntimes[sessionId];
            try
            {
                _draftStore.Save(sessionId, new(runtime.Draft, runtime.Queue.ToList()));
                _dirtyDraftSessions.Remove(sessionId);
            }
            catch (Exception error)
            {
                // Keep the dirty snapshot for another edit, navigation or close.
                if (!_closed && !_draftSaveErrorShown)
                {
                    _draftSaveErrorShown = true;
                    ShowError($"Cannot save your draft: {error.Message}. It is still available in this window.");
                }
            }
        }
        if (_dirtyDraftSessions.Count == 0) _draftSaveErrorShown = false;
    }

    private void SwitchComposerSession(string sessionId)
    {
        if (sessionId == _currentSessionId) return;
        CaptureComposerDraft();
        SaveComposerDrafts();
        _currentSessionId = sessionId;
        var runtime = RuntimeFor(sessionId);
        if (!runtime.DraftLoaded)
        {
            runtime.DraftLoaded = true;
            try
            {
                var saved = _draftStore.Load(RuntimeKey(sessionId));
                runtime.Draft = saved.Draft;
                foreach (var item in saved.Queue) runtime.Queue.Enqueue(item);
                // Restoring a window must never execute pending work by itself.
                runtime.QueuePaused = runtime.Queue.Count > 0;
            }
            catch (Exception error)
            {
                ShowError($"Cannot restore this conversation's draft: {error.Message}. The saved file has been left unchanged.");
            }
        }
        _restoringComposer = true;
        try
        {
            Composer.Text = runtime.Draft.Text;
            Composer.CaretIndex = Math.Clamp(runtime.Draft.CaretIndex, 0, runtime.Draft.Text.Length);
            _attachments.Clear();
            if (runtime.Draft.Attachment is { } attachment) _attachments.Add(attachment);
            RefreshAttachmentBar();
            _historyPosition = null;
        }
        finally { _restoringComposer = false; }
    }

    private void CompleteComposerSubmission(string sessionId, string originalText, AttachedFile? attachment)
    {
        var runtime = RuntimeFor(sessionId);
        if (runtime.Draft.Text != originalText || runtime.Draft.Attachment != attachment) return;
        runtime.Draft = ComposerDraft.Empty;
        MarkDraftChanged(sessionId);
        // The user may have navigated or edited while an asynchronous action ran.
        if (_currentSessionId == sessionId && Composer.Text == originalText
            && _attachments.FirstOrDefault() == attachment)
        {
            _restoringComposer = true;
            try
            {
                Composer.Clear();
                _attachments.Clear();
                RefreshAttachmentBar();
            }
            finally { _restoringComposer = false; }
        }
        SaveComposerDrafts();
    }

    private async Task PauseAndInterruptAsync()
    {
        RuntimeFor(_currentSessionId).QueuePaused = true;
        RefreshComposer();
        await SendAsync(new() { ["op"] = "interrupt" });
    }

    private async void ResumeQueue_Click(object? sender, RoutedEventArgs e) => await RunUiAsync(async () =>
    {
        RuntimeFor(_currentSessionId).QueuePaused = false;
        RefreshComposer();
        await SendNextQueuedAsync();
    });

    private void ClearQueue_Click(object? sender, RoutedEventArgs e)
    {
        if (_dispatching) return;
        CurrentQueue.Clear();
        RuntimeFor(_currentSessionId).QueuePaused = false;
        MarkDraftChanged(_currentSessionId);
        SaveComposerDrafts();
        RefreshComposer();
    }
}
