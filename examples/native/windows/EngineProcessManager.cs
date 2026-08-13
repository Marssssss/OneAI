// EngineProcessManager.cs — Windows mirror of the macOS
// EngineProcessManager.swift. Owns the spawned `oneai app-server` sidecar
// for the Windows app's sidecar transport (Codex model: the desktop frontend
// that can spawn a process owns the spawn — no manual server start).
//
// Locates the `oneai` binary (bundled next to the app exe first, then PATH),
// spawns `oneai app-server --listen pipe://oneai-<pid>` (named pipe, the
// Windows ipc transport), waits for the pipe, then hands the pipe name to a
// OneAiRpcClient. On unexpected exit, restart with exponential backoff.
//
// Not wired into the WinUI ChatViewModel (deferred per plan) — client + spawn
// infra + a smoke Program.cs round-trip. Windows C# is not compiled in this
// repo; verify on a Windows host.

using System;
using System.Diagnostics;
using System.IO;
using System.Threading;
using System.Threading.Tasks;

namespace OneAI.Native;

public interface IEngineProcessManagerDelegate
{
    Task OnStarted(OneAiRpcClient client);
    void OnFailed(Exception error);
}

public sealed class EngineProcessManager : IDisposable
{
    private readonly IEngineProcessManagerDelegate _delegate;
    private Process? _process;
    private OneAiRpcClient? _client;
    private string _pipeName = "";
    private double _backoffMs = 500;
    private Timer? _restartTimer;
    private bool _started, _stopped;

    public EngineProcessManager(IEngineProcessManagerDelegate @delegate) => _delegate = @delegate;

    /// <summary>Resolve the oneai binary: next to the app exe first (bundled),
    /// then PATH.</summary>
    public static string? ResolveOneaiBin()
    {
        var exeDir = AppDomain.CurrentDomain.BaseDirectory;
        var bundled = Path.Combine(exeDir, "bin", "oneai.exe");
        if (File.Exists(bundled)) return bundled;
        // PATH lookup.
        var path = Environment.GetEnvironmentVariable("PATH") ?? "";
        foreach (var dir in path.Split(Path.PathSeparator))
        {
            if (string.IsNullOrWhiteSpace(dir)) continue;
            var candidate = Path.Combine(dir.Trim(), "oneai.exe");
            if (File.Exists(candidate)) return candidate;
        }
        return null;
    }

    public void Start()
    {
        if (_started) return;
        _started = true;
        Spawn();
    }

    public void Dispose()
    {
        _stopped = true;
        _restartTimer?.Dispose();
        _client?.Dispose();
        _client = null;
        try { _process?.Kill(); } catch { }
        _process = null;
    }

    private void Spawn()
    {
        if (_stopped) return;
        var bin = ResolveOneaiBin();
        if (bin is null)
        {
            _delegate.OnFailed(new EngineProcessException("oneai binary not found (bundle or PATH)"));
            ScheduleRestart();
            return;
        }

        // Ephemeral pipe name so it never clashes with a user's manually
        // started app-server pipe. oneai-<pid> mirrors the macOS socket.
        _pipeName = $"oneai-{Process.GetCurrentProcess().Id}";

        var p = new Process
        {
            StartInfo = new ProcessStartInfo
            {
                FileName = bin,
                Arguments = $"app-server --listen pipe://{_pipeName}",
                UseShellExecute = false,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
                // Inherit env so the app-server reads ONEAI_API_KEY / base url / model.
            },
            EnableRaisingEvents = true,
        };
        p.Exited += (_, _) => HandleExit();
        try
        {
            p.Start();
            _process = p;
            _ = Task.Run(() => WaitForPipeThenConnect());
        }
        catch (Exception e)
        {
            _delegate.OnFailed(e);
            ScheduleRestart();
        }
    }

    private async Task WaitForPipeThenConnect(int attempt = 0)
    {
        for (; attempt < 100; attempt++) // ~10s @100ms
        {
            if (_stopped) return;
            if (NamedPipeIsAvailable(_pipeName)) break;
            await Task.Delay(100);
            if (attempt == 99)
            {
                _delegate.OnFailed(new EngineProcessException("app-server pipe timeout"));
                ScheduleRestart();
                return;
            }
        }
        try
        {
            var c = new OneAiRpcClient(_pipeName);
            await c.ConnectAsync();
            _client = c;
            _backoffMs = 500; // reset after a healthy start
            await _delegate.OnStarted(c);
        }
        catch (Exception e)
        {
            _delegate.OnFailed(e);
            ScheduleRestart();
        }
    }

    private void HandleExit()
    {
        _client?.Dispose();
        _client = null;
        if (_stopped) return;
        ScheduleRestart();
    }

    private void ScheduleRestart()
    {
        if (_stopped) return;
        var delay = _backoffMs;
        _backoffMs = Math.Min(_backoffMs * 2, 30_000); // cap 30s
        _restartTimer = new Timer(_ => Spawn(), null, (int)delay, Timeout.Infinite);
    }

    // Probe whether the named pipe exists (the app-server binds async).
    private static bool NamedPipeIsAvailable(string name)
    {
        var path = $@"\\.\pipe\{name}";
        return Directory.GetFiles(@"\\.\pipe\", name).Length > 0
            || File.Exists(path); // File.Exists on a pipe name returns true once bound
    }
}

public sealed class EngineProcessException : Exception
{
    public EngineProcessException(string message) : base(message) { }
}
