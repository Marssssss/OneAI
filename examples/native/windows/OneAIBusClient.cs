// OneAIBusClient.cs
// Windows native frontend for `oneai serve` (the engine-bus sidecar).
//
// A Directive writer + Yield reader over a named pipe. This replaces the
// hand-written C# extern-C facade against oneai-uniffi's c_facade.rs — see
// the P3 plan's "Deferred" section: build the WinUI3 app against this client,
// verify a real turn + approval roundtrip, then collapse c_facade.rs to the
// mobile-only 3-symbol form (P4).
//
// Wire framing: one newline-terminated JSON object per message, same as
// oneai-bus's `serialize_yield` / `serialize_directive`. See
// crates/oneai-bus/src/protocol.rs for the canonical `kind` tags.
//
// On Windows the sidecar listens on a named pipe. The pipe NAME is the
// flattened socket path (`oneai_supervisor::transport::to_pipe_name`). The
// cleanest cross-plate default: start the sidecar with
//   `oneai serve --socket oneai-serve`
// which on Windows becomes the pipe `\\.\pipe\oneai-serve`. Pass that name
// (without the `\\.\pipe\` prefix) to the client, or pass the full path — the
// client normalizes both.
//
// This is a skeleton — connect/send/receive/approval loop only.

using System;
using System.IO;
using System.IO.Pipes;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;

namespace OneAI.Native;

/// <summary>One yield the engine sent, decoded enough to dispatch on <c>kind</c>.</summary>
public sealed class OneAIBusClient : IDisposable
{
    private readonly string _pipeName;
    private NamedPipeClientStream? _pipe;
    private StreamReader? _reader;
    private StreamWriter? _writer;
    private CancellationTokenSource? _cts;

    /// <param name="pipeName">Pipe name without the <c>\\.\pipe\</c> prefix
    /// (e.g. <c>oneai-serve</c>), or the full path.</param>
    public OneAIBusClient(string pipeName = "oneai-serve")
    {
        const string prefix = @"\\.\pipe\";
        _pipeName = pipeName.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)
            ? pipeName[prefix.Length..]
            : pipeName;
    }

    public event Action<JsonElement>? OnYield;
    public event Action<Exception?>? OnClosed;

    public async Task ConnectAsync(CancellationToken ct = default)
    {
        _pipe = new NamedPipeClientStream(
            ".",
            _pipeName,
            PipeDirection.InOut,
            PipeOptions.Asynchronous);
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
        GC.SuppressFinalize(this);
    }

    // ── Sending directives ────────────────────────────────────────────────

    /// <summary>Send a <c>Directive</c> as one JSON line.</summary>
    public async Task SendDirectiveAsync(object directive, CancellationToken ct = default)
    {
        if (_writer is null) throw new InvalidOperationException("not connected");
        var json = JsonSerializer.Serialize(directive);
        await _writer.WriteLineAsync(json.AsMemory(), ct);
        await _writer.FlushAsync(ct);
    }

    public Task SendUserMessageAsync(string text, CancellationToken ct = default) =>
        SendDirectiveAsync(new
        {
            kind = "user_message",
            content = new[] { new { type = "text", text } },
        }, ct);

    public Task SendInterruptAsync(string reason, CancellationToken ct = default) =>
        SendDirectiveAsync(new
        {
            kind = "interrupt",
            reason = new { Custom = new { reason } },
        }, ct);

    /// <summary>Reply to an <c>approval_request</c> yield.</summary>
    public Task RespondToApprovalAsync(string requestId, bool proceed, CancellationToken ct = default)
    {
        // InteractionResponse is externally-tagged: the Proceed unit variant
        // serializes as the bare string "Proceed"; Abort as an object.
        object response = proceed ? (object)"Proceed" : new { Abort = new { reason = "user denied" } };
        return SendDirectiveAsync(new
        {
            kind = "approve",
            request_id = requestId,
            response,
        }, ct);
    }

    // ── Receiving yields ──────────────────────────────────────────────────

    private async Task ReceiveLoop()
    {
        var token = _cts!.Token;
        try
        {
            while (!token.IsCancellationRequested)
            {
                var line = await _reader!.ReadLineAsync(token);
                if (line is null) { OnClosed?.Invoke(null); break; }
                if (string.IsNullOrWhiteSpace(line)) continue;
                try
                {
                    using var doc = JsonDocument.Parse(line);
                    OnYield?.Invoke(doc.RootElement.Clone());
                }
                catch (JsonException ex)
                {
                    OnYield?.Invoke(JsonSerializer.SerializeToElement(new
                    {
                        kind = "error",
                        recoverable = true,
                        message = $"malformed yield line: {ex.Message}",
                    }));
                }
            }
        }
        catch (OperationCanceledException) { /* shutting down */ }
        catch (Exception ex) { OnClosed?.Invoke(ex); }
    }
}
