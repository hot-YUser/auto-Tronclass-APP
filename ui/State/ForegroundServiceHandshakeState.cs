namespace Ui;

/// <summary>
/// Android foreground-service handshake ownership. Each start receives an immutable lease; starting a
/// new generation or destroying/stopping the service cancels the old lease. Every externally visible
/// effect must pass through this state so a stale continuation cannot commit success, stop, notify, or
/// dispatch another core command. The operation is serialized via an owned task chain so a stale
/// generation's non-cancellable core mutation cannot run concurrently with a newer generation, yet a
/// hung old task does not block the new generation forever (bounded wait).
/// </summary>
internal sealed class ForegroundServiceHandshakeState
{
    internal sealed class Lease : IDisposable
    {
        readonly CancellationTokenSource _source = new();
        int _refs;
        int _disposed;

        internal Lease(long generation)
        {
            Generation = generation;
            Cancellation = _source.Token;
        }

        public long Generation { get; }
        public CancellationToken Cancellation { get; }

        internal void Cancel() => _source.Cancel();
        internal void AddRef() => Interlocked.Increment(ref _refs);
        internal void Release() => Interlocked.Decrement(ref _refs);
        internal int Refs => Volatile.Read(ref _refs);
        internal bool IsDisposed => Volatile.Read(ref _disposed) != 0;

        public void Dispose()
        {
            if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
            _source.Dispose();
        }
    }

    readonly object _gate = new();
    long _nextGeneration;
    Lease? _current;
    bool _ready;
    Task? _activeOperation;
    TaskCompletionSource<object?>? _pendingCompletion;

    public Lease Begin()
    {
        Lease? old;
        lock (_gate)
        {
            old = _current;
            _current = new Lease(++_nextGeneration);
            _ready = false;
            old?.Cancel();
            if (old is not null && old.Refs == 0)
                old.Dispose();
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

    public async Task RunAsync(Lease lease, Func<CancellationToken, Task> operation)
    {
        var tcs = new TaskCompletionSource<object?>(TaskCreationOptions.RunContinuationsAsynchronously);
        Task? toAwait = null;
        lock (_gate)
        {
            ThrowIfStaleLocked(lease);
            if (_activeOperation is not null)
                toAwait = _activeOperation;
            lease.AddRef();
            _activeOperation = tcs.Task;
            _pendingCompletion = tcs;
        }

        try
        {
            if (toAwait is not null)
            {
                var timeout = TimeSpan.FromSeconds(30);
                using var cts = new CancellationTokenSource(timeout);
                using var linked = CancellationTokenSource.CreateLinkedTokenSource(lease.Cancellation, cts.Token);
                try { await toAwait.WaitAsync(linked.Token).ConfigureAwait(false); } catch { }
                lock (_gate) ThrowIfStaleLocked(lease);
                lease.Cancellation.ThrowIfCancellationRequested();
            }

            Task task;
            lock (_gate)
            {
                ThrowIfStaleLocked(lease);
                task = operation(lease.Cancellation);
                ObserveFault(task);
            }
            await task.WaitAsync(lease.Cancellation).ConfigureAwait(false);
            tcs.TrySetResult(null);
        }
        catch (OperationCanceledException)
        {
            tcs.TrySetCanceled(lease.Cancellation);
            throw;
        }
        catch (Exception ex)
        {
            tcs.TrySetException(ex);
            throw;
        }
        finally
        {
            lease.Release();
            TryDisposeIfQuiescent(lease);
            lock (_gate)
            {
                if (ReferenceEquals(_pendingCompletion, tcs))
                    _pendingCompletion = null;
            }
        }
    }

    public async Task<T> RunAsync<T>(Lease lease, Func<CancellationToken, Task<T>> operation)
    {
        var tcs = new TaskCompletionSource<object?>(TaskCreationOptions.RunContinuationsAsynchronously);
        Task? toAwait = null;
        lock (_gate)
        {
            ThrowIfStaleLocked(lease);
            if (_activeOperation is not null)
                toAwait = _activeOperation;
            lease.AddRef();
            _activeOperation = tcs.Task;
            _pendingCompletion = tcs;
        }

        try
        {
            if (toAwait is not null)
            {
                var timeout = TimeSpan.FromSeconds(30);
                using var cts = new CancellationTokenSource(timeout);
                using var linked = CancellationTokenSource.CreateLinkedTokenSource(lease.Cancellation, cts.Token);
                try { await toAwait.WaitAsync(linked.Token).ConfigureAwait(false); } catch { }
                lock (_gate) ThrowIfStaleLocked(lease);
                lease.Cancellation.ThrowIfCancellationRequested();
            }

            Task<T> task;
            lock (_gate)
            {
                ThrowIfStaleLocked(lease);
                task = operation(lease.Cancellation);
                ObserveFault(task);
            }
            var result = await task.WaitAsync(lease.Cancellation).ConfigureAwait(false);
            tcs.TrySetResult(null);
            return result;
        }
        catch (OperationCanceledException)
        {
            tcs.TrySetCanceled(lease.Cancellation);
            throw;
        }
        catch (Exception ex)
        {
            tcs.TrySetException(ex);
            throw;
        }
        finally
        {
            lease.Release();
            TryDisposeIfQuiescent(lease);
            lock (_gate)
            {
                if (ReferenceEquals(_pendingCompletion, tcs))
                    _pendingCompletion = null;
            }
        }
    }

    public Task RunAsync(Lease lease, Func<Task> operation) =>
        RunAsync(lease, _ => operation());

    public Task<T> RunAsync<T>(Lease lease, Func<Task<T>> operation) =>
        RunAsync<T>(lease, _ => operation());

    public Task DelayAsync(Lease lease, TimeSpan delay)
    {
        lock (_gate)
        {
            ThrowIfStaleLocked(lease);
            lease.AddRef();
        }
        Task task;
        try
        {
            task = Task.Delay(delay, lease.Cancellation);
        }
        catch
        {
            lease.Release();
            TryDisposeIfQuiescent(lease);
            throw;
        }
        _ = task.ContinueWith(
            _ =>
            {
                lease.Release();
                TryDisposeIfQuiescent(lease);
            },
            CancellationToken.None,
            TaskContinuationOptions.ExecuteSynchronously,
            TaskScheduler.Default);
        return task;
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
        Lease? old;
        lock (_gate)
        {
            old = _current;
            _current = null;
            _ready = false;
            old?.Cancel();
            if (old is not null && old.Refs == 0)
                old.Dispose();
        }
    }

    void TryDisposeIfQuiescent(Lease lease)
    {
        bool shouldDispose = false;
        lock (_gate)
        {
            if (lease.Refs == 0 && (lease.Cancellation.IsCancellationRequested || !ReferenceEquals(_current, lease)) && !lease.IsDisposed)
                shouldDispose = true;
        }
        if (shouldDispose) lease.Dispose();
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
        if (current is not null)
        {
            current.Cancel();
            if (current.Refs == 0)
                current.Dispose();
        }
    }
}
