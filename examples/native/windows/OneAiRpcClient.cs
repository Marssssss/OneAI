// OneAiRpcClient.cs — Windows JSON-RPC 2.0 client for `oneai app-server`
// (the sidecar transport), over a named pipe. Mirrors the macOS
// OneAiRpcClient.swift and the VS Code server.ts: same JSON-RPC envelope,
// same id correlation, same `event` notification dispatch — only the
// transport differs (named pipe vs Unix domain socket vs stdio).
//
// The Windows app's default transport stays in-process FFI (c_facade /
// oneai_native.dll). This client is the out-of-process option: the app
// spawns `oneai app-server --listen pipe://oneai-<pid>` (see
// EngineProcessManager.cs) and routes turns/approvals/group/scenario
// through here. Per the plan, the WinUI ChatViewModel rewiring is deferred;
// this is the client + spawn infra, plus a smoke Program.cs round-trip.
//
// Framing: newline-terminated JSON (matches `serve_ipc` on Windows, which
// uses the same line framing as stdio). The pipe NAME is the flattened
// socket path (`oneai_supervisor::transport::to_pipe_name`) — pass the same
// name the app-server was started with.
//
// Windows C# is not compiled in this repo (no Windows host) — this mirrors
// the Swift/TS clients structurally and is written to be idiomatic + build
// on a Windows machine. Verify there.

using System;
using System.Collections.Concurrent;
using System.IO.Pipes;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;

namespace OneAI.Native;

/// <summary>One <c>event</c> notification the engine sent, decoded enough to
/// dispatch on <c>kind</c>.</summary>
public sealed class OneAiEvent
{
    public string Kind { get; init; } = "";
    public JsonElement Params { get; init; }
}

/// <summary>A JSON-RPC 2.0 client over a Windows named pipe (the app-server
/// ipc transport). <see cref="Call"/> is awaitable (id correlation); events
/// fire on <see cref="OnEvent"/>.</summary>
public sealed class OneAiRpcClient : IDisposable
{
    private readonly string _pipeName;
    private NamedPipeClientStream? _pipe;
    private StreamReader? _reader;
    private StreamWriter? _writer;
    private CancellationTokenSource? _cts;
    private int _nextId = 1;
    private readonly ConcurrentDictionary<int, TaskCompletionSource<JsonElement>> _pending = new();

    public event Action<OneAiEvent>? OnEvent;
    public event Action<Exception?>? OnClosed;

    /// <param name="pipeName">Pipe name without the <c>\\.\pipe\</c> prefix
    /// (e.g. <c>oneai-12345</c>), matching the name the app-server was
    /// started with via <c>--listen pipe://oneai-12345</c>.</param>
    public OneAiRpcClient(string pipeName) => _pipeName = NormalizeName(pipeName);

    private static string NormalizeName(string name)
    {
        const string prefix = @"\\.\pipe\";
        return name.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)
            ? name[prefix.Length..]
            : name;
    }

    public async Task ConnectAsync(CancellationToken ct = default)
    {
        _pipe = new NamedPipeClientStream(
            ".", _pipeName, PipeDirection.InOut, PipeOptions.Asynchronous);
        await _pipe.ConnectAsync(ct);
        _reader = new StreamReader(_pipe, new UTF8Encoding(false));
        _writer = new StreamWriter(_pipe, new UTF8Encoding(false)) { NewLine = "\n", AutoFlush = true };
        _cts = CancellationTokenSource.CreateLinkedTokenSource(ct);
        _ = Task.Run(ReceiveLoop, _cts.Token);
    }

    public void Dispose()
    {
        _cts?.Cancel();
        _writer?.Dispose();
        _reader?.Dispose();
        _pipe?.Dispose();
        // Reject anything still pending.
        foreach (var kv in _pending)
        {
            kv.Value.TrySetException(new InvalidOperationException("rpc client disposed"));
        }
        _pending.Clear();
        GC.SuppressFinalize(this);
    }

    // ── Sending requests ────────────────────────────────────────────────

    /// <summary>Send a JSON-RPC request; resolves with the <c>result</c>
    /// element. For <c>turn/run</c> that's <c>{turn_id}</c>; ack methods
    /// <c>{ok:true}</c>; <c>scenario/validate</c> <c>{ok, errors}</c>.</summary>
    public async Task<JsonElement> Call(string method, object? parameters, CancellationToken ct = default)
    {
        if (_writer is null) throw new InvalidOperationException("not connected");
        int id = Interlocked.Increment(ref _nextId);
        var tcs = new TaskCompletionSource<JsonElement>(TaskCreationOptions.RunContinuationsAsynchronously);
        _pending[id] = tcs;

        var payload = new
        {
            jsonrpc = "2.0",
            id,
            method,
            @params = parameters,
        };
        var json = JsonSerializer.Serialize(payload, payload.GetType());
        await _writer.WriteLineAsync(json.AsMemory(), ct);
        await _writer.FlushAsync(ct);

        // Reject on cancellation so the pending entry is cleaned up.
        await using var _ = ct.Register(() =>
        {
            if (_pending.TryRemove(id, out var orphan))
                orphan.TrySetCanceled(ct);
        });
        return await tcs.Task; // throws on rpc error or disconnect
    }

    private async Task ReceiveLoop()
    {
        var token = _cts!.Token;
        try
        {
            while (!token.IsCancellationRequested && _reader is not null)
            {
                var line = await _reader.ReadLineAsync(token);
                if (line is null) break; // pipe closed
                if (string.IsNullOrWhiteSpace(line)) continue;
                try
                {
                    using var doc = JsonDocument.Parse(line);
                    HandleMessage(doc.RootElement);
                }
                catch
                {
                    // Malformed line — skip (mirrors the other clients).
                }
            }
        }
        catch
        {
            // reader errored — fall through to close.
        }
        DrainPendingAndClose();
    }

    private void HandleMessage(JsonElement msg)
    {
        // Response to a pending call (has id + result/error).
        if (msg.TryGetProperty("id", out var idEl)
            && (msg.TryGetProperty("result", out _) || msg.TryGetProperty("error", out _)))
        {
            var id = idEl.GetInt32();
            if (!_pending.TryRemove(id, out var tcs)) return;
            if (msg.TryGetProperty("error", out var err))
            {
                var code = err.TryGetProperty("code", out var c) ? c.GetInt32() : -1;
                var message = err.TryGetProperty("message", out var m) ? m.GetString() ?? "rpc error" : "rpc error";
                tcs.TrySetException(new OneAiRpcException(code, message));
            }
            else if (msg.TryGetProperty("result", out var result))
            {
                tcs.TrySetResult(result.Clone());
            }
            return;
        }
        // Notification (no id) — the app-server's single outbound method `event`.
        if (msg.TryGetProperty("method", out var m) && m.GetString() == "event"
            && msg.TryGetProperty("params", out var p))
        {
            var kind = p.TryGetProperty("kind", out var k) ? k.GetString() ?? "" : "";
            OnEvent?.Invoke(new OneAiEvent { Kind = kind, Params = p.Clone() });
        }
    }

    private void DrainPendingAndClose()
    {
        foreach (var kv in _pending)
        {
            kv.Value.TrySetException(new InvalidOperationException("rpc connection closed"));
        }
        _pending.Clear();
        OnClosed?.Invoke(null);
    }
}

/// <summary>An error returned by the app-server (JSON-RPC <c>error</c>).</summary>
public sealed class OneAiRpcException : Exception
{
    public int Code { get; }
    public OneAiRpcException(int code, string message) : base($"{message} (code {code})") => Code = code;
}
