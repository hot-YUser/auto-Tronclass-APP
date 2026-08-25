namespace Ui;

/// <summary>
/// Android foreground-service handshake ownership. Each start receives an immutable lease; starting a
/// new generation or destroying/stopping the service cancels the old lease. Every externally visible
/// effect must pass through this state so a stale continuation cannot commit success, stop, notify, or
/// dispatch another core command.
/// </summary>
internal sealed class ForegroundServiceHandshakeState
{
    internal sealed class Lease
    {
        readonly CancellationTokenSource _source = new();

        internal Lease(long generation)
        {
            Generation = generation;
            Cancellation = _source.Token;
        }

        public long Generation { get; }
        public CancellationToken Cancellation { get; }

        internal void CancelAndDispose()
        {
            _source.Cancel();
            _source.Dispose();
        }
    }

    readonly object _gate = new();
    long _nextGeneration;
    Lease? _current;
    bool _ready;

    public Lease Begin()
    {
        lock (_gate)
        {
            _current?.CancelAndDispose();
            _current = new Lease(++_nextGeneration);
            _ready = false;
            return _current;
        }
    }

    public bool IsCurrent(Lease lease)
    {
        lock (_gate) return IsCurrentLocked(lease);
    }

    public bool IsReady(Lease lease)
    {
        lock (_gate) return IsCurrentLocked(lease) && _ready;
    }

    public async Task RunAsync(Lease lease, Func<Task> operation)
    {
        Task task;
        lock (_gate)
        {
            ThrowIfStaleLocked(lease);
            task = operation();
            ObserveFault(task);
        }
        await task.WaitAsync(lease.Cancellation).ConfigureAwait(false);
    }

    public async Task<T> RunAsync<T>(Lease lease, Func<Task<T>> operation)
    {
        Task<T> task;
        lock (_gate)
        {
            ThrowIfStaleLocked(lease);
            task = operation();
            ObserveFault(task);
        }
        return await task.WaitAsync(lease.Cancellation).ConfigureAwait(false);
    }

    public Task DelayAsync(Lease lease, TimeSpan delay)
    {
        lock (_gate)
        {
            ThrowIfStaleLocked(lease);
            return Task.Delay(delay, lease.Cancellation);
        }
    }

    public bool TryRun(Lease lease, Action effect)
    {
        lock (_gate)
        {
            if (!IsCurrentLocked(lease)) return false;
            effect();
            return true;
        }
    }

    public bool TrySucceed(Lease lease, Action effect)
    {
        lock (_gate)
        {
            if (!IsCurrentLocked(lease)) return false;
            _ready = true;
            effect();
            return true;
        }
    }

    public bool TryStop(Lease lease, Action effect)
    {
        lock (_gate)
        {
            if (!IsCurrentLocked(lease)) return false;
            InvalidateLocked();
            effect();
            return true;
        }
    }

    public bool TryStopCurrent(Action effect)
    {
        lock (_gate)
        {
            if (_current is null) return false;
            InvalidateLocked();
            effect();
            return true;
        }
    }

    public void Destroy()
    {
        lock (_gate) InvalidateLocked();
    }

    static void ObserveFault(Task task) =>
        _ = task.ContinueWith(
            static completed => _ = completed.Exception,
            CancellationToken.None,
            TaskContinuationOptions.OnlyOnFaulted | TaskContinuationOptions.ExecuteSynchronously,
            TaskScheduler.Default);

    bool IsCurrentLocked(Lease lease) =>
        ReferenceEquals(_current, lease) && !lease.Cancellation.IsCancellationRequested;

    void ThrowIfStaleLocked(Lease lease)
    {
        if (!IsCurrentLocked(lease)) throw new OperationCanceledException(lease.Cancellation);
    }

    void InvalidateLocked()
    {
        var current = _current;
        _current = null;
        _ready = false;
        current?.CancelAndDispose();
    }
}
