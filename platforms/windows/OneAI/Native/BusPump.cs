// BusPump — the Windows C# counterpart of examples/native/ios/OneAIBusPump.swift
// and examples/native/android/OneAIBusPump.kt. Drives the engine through the 3
// extern "C" symbols P4 collapsed the facade to (see OneAiNative.cs).
//
// THE poll buffer behind oneai_poll_yield is THREAD-LOCAL and is overwritten on
// every call from the same thread. The pump therefore owns exactly ONE
// dedicated background Thread that performs every PollYield(); nothing else may
// call PollYield. Each drained line is copied to a managed string immediately,
// parsed for its "kind", and routed to YieldReceived (on the poll thread — the
// subscriber marshals to the UI thread / completes its own TaskCompletionSources).
//
// A 20fps cadence (50ms) matches the iOS/Android pumps and the StreamCoalescer.

using System;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Threading;

namespace OneAI.Native;

/// <summary>One yield the engine produced, decoded enough to dispatch on
/// <c>kind</c>. <see cref="Json"/> is a standalone copy (safe to hold after
/// the pump's thread-local buffer is reused).</summary>
public readonly struct BusYield
{
    public string Kind { get; }
    public JsonElement Json { get; }
    public BusYield(string kind, JsonElement json) { Kind = kind; Json = json; }
}

public sealed class BusPump : IDisposable
{
    /// <summary>Raised on the pump's poll thread for every yield. Subscribers
    /// must marshal to the UI thread themselves (DispatcherQueue) or complete
    /// thread-safe primitives (TaskCompletionSource).</summary>
    public event Action<BusYield>? YieldReceived;

    private Thread? _pollThread;
    private volatile bool _running;
    private readonly object _startLock = new();

    // ── Poll loop ─────────────────────────────────────────────────────

    /// <summary>Start the 20fps poll loop. Idempotent.</summary>
    public void Start()
    {
        lock (_startLock)
        {
            if (_running) return;
            _running = true;
            _pollThread = new Thread(PollLoop) { IsBackground = true, Name = "oneai.bus.pump.poll" };
            _pollThread.Start();
        }
    }

    private void PollLoop()
    {
        while (_running)
        {
            bool drainedAny = false;
            try
            {
                // Drain every pending yield (non-blocking) before sleeping so a
                // burst of fragments doesn't lag a frame behind.
                while (_running)
                {
                    IntPtr p = OneAiNative.PollYield();
                    if (p == IntPtr.Zero) break;               // no yield pending
                    string? line = OneAiNative.YieldPtrToString(p); // copy NOW
                    drainedAny = true;
                    if (line is null) continue;
                    Dispatch(line);
                }
            }
            catch (Exception)
            {
                // A bad yield must never kill the poll thread — skip it and keep
                // draining; the engine is still running.
            }
            // Sleep only when the queue was empty; a drained burst loops again
            // immediately so streaming stays tight.
            if (!drainedAny) Thread.Sleep(50);
        }
    }

    private void Dispatch(string line)
    {
        JsonDocument doc;
        try { doc = JsonDocument.Parse(line); }
        catch (JsonException) { return; }
        using (doc)
        {
            string kind = doc.RootElement.TryGetProperty("kind", out var k) && k.ValueKind == JsonValueKind.String
                ? k.GetString() ?? ""
                : "";
            if (kind.Length == 0) return;
            // Clone detaches from `doc`'s pooled memory so the element stays valid
            // after we dispose the document.
            var json = doc.RootElement.Clone();
            YieldReceived?.Invoke(new BusYield(kind, json));
        }
    }

    // ── Sending directives ────────────────────────────────────────────

    /// <summary>Submit a full Directive JSON object (incl. "kind"). Returns the
    /// submit status (0 = ok; see OneAiNative codes).</summary>
    public int Submit(string directiveJson) => OneAiNative.SubmitDirective(directiveJson);

    /// <summary>Submit a <c>Directive::Init { config }</c> to build the engine +
    /// bus + pump. Call once at launch (and again after Shutdown to rebuild).
    /// <paramref name="configJson"/> is the provider/config object body.</summary>
    public int Init(string configJson)
    {
        var node = JsonNode.Parse(configJson) ?? new JsonObject();
        var envelope = new JsonObject { ["kind"] = "init", ["config"] = node };
        return Submit(envelope.ToJsonString());
    }

    public int SendUserMessage(string text)
    {
        var envelope = new JsonObject
        {
            ["kind"] = "user_message",
            ["content"] = new JsonArray { new JsonObject { ["type"] = "text", ["text"] = text } },
        };
        return Submit(envelope.ToJsonString());
    }

    public int SendInterrupt(string reason)
    {
        var envelope = new JsonObject
        {
            ["kind"] = "interrupt",
            ["reason"] = new JsonObject { ["Custom"] = new JsonObject { ["reason"] = reason } },
        };
        return Submit(envelope.ToJsonString());
    }

    // ── Session lifecycle directives ──────────────────────────────────

    /// <summary>List saved conversations → resolves a <c>session_list</c> yield.</summary>
    public int ListSessions() => Submit("{\"kind\":\"list_sessions\"}");

    /// <summary>Start a fresh single-agent session → <c>session_created</c> yield.</summary>
    public int CreateSession() => Submit("{\"kind\":\"create_session\"}");

    /// <summary>Load a saved session by id → <c>session_loaded</c> yield (carries
    /// the message history for replay).</summary>
    public int LoadSession(string id)
    {
        var envelope = new JsonObject { ["kind"] = "load_session", ["id"] = id };
        return Submit(envelope.ToJsonString());
    }

    /// <summary>Delete a saved session → <c>session_deleted</c> yield.</summary>
    public int DeleteSession(string id)
    {
        var envelope = new JsonObject { ["kind"] = "delete_session", ["id"] = id };
        return Submit(envelope.ToJsonString());
    }

    // ── Group-chat (multi-agent scenario) directives ──────────────────

    /// <summary>Build a group chat from a scenario (the
    /// <c>start_group_chat</c> Directive). <paramref name="scenarioJson"/> is a
    /// serialized ScenarioSpecDto (BusGroupScenario shape).</summary>
    public int StartGroupChat(string scenarioJson)
    {
        var scenario = JsonNode.Parse(scenarioJson) ?? new JsonObject();
        var envelope = new JsonObject { ["kind"] = "start_group_chat", ["scenario"] = scenario };
        return Submit(envelope.ToJsonString());
    }

    /// <summary>Run the scenario's configured opener turn (no user message).</summary>
    public int GroupStart() => Submit("{\"kind\":\"group_start\"}");

    /// <summary>Append the user's message and run the round's speakers.</summary>
    public int GroupUserMessage(string userInput)
    {
        var envelope = new JsonObject { ["kind"] = "group_user_message", ["user_input"] = userInput };
        return Submit(envelope.ToJsonString());
    }

    /// <summary>Hot-swap the turn policy to a fixed scripted order.
    /// <paramref name="orderJsonArray"/> is a JSON string array, e.g. <c>["coach"]</c>.</summary>
    public int GroupSetScriptedOrder(string orderJsonArray)
    {
        var order = JsonNode.Parse(orderJsonArray) ?? new JsonArray();
        var envelope = new JsonObject { ["kind"] = "group_set_scripted_order", ["order"] = order };
        return Submit(envelope.ToJsonString());
    }

    /// <summary>Reply to an <c>approval_request</c> yield. <c>proceed == true</c>
    /// sends <c>InteractionResponse::Proceed</c> (a bare "Proceed" JSON string);
    /// false sends <c>Abort</c>.</summary>
    public int RespondApproval(string requestId, bool proceed)
    {
        JsonNode response = proceed
            ? JsonValue.Create("Proceed")!
            : new JsonObject { ["Abort"] = new JsonObject { ["reason"] = "user denied" } };
        var envelope = new JsonObject
        {
            ["kind"] = "approve",
            ["request_id"] = requestId,
            ["response"] = response,
        };
        return Submit(envelope.ToJsonString());
    }

    /// <summary>Shut the engine down — submits <c>Directive::Shutdown</c>, aborts
    /// the pump, and stops polling. Safe to call once; the poll thread exits.</summary>
    public void Shutdown()
    {
        _running = false;
        try { OneAiNative.Shutdown(); } catch (DllNotFoundException) { /* dll gone on teardown */ }
        // Give the poll thread a moment to observe _running==false and exit.
        _pollThread?.Join(250);
        _pollThread = null;
    }

    public void Dispose() => Shutdown();
}
