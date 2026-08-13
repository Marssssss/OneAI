// Program.cs — smoke test for the Windows sidecar JSON-RPC path.
// Spawns `oneai app-server --listen pipe://...` via EngineProcessManager,
// sends `scenario/list`, prints the seeded scenario names. Verifies the
// pipe transport + JSON-RPC framing + the scenario/* surface end-to-end.
//
// Build on Windows (oneai.exe on PATH or bundled), then:
//   dotnet run --project examples/native/windows/smoke
// Not compiled in this repo (no Windows host).

using System;
using System.Threading.Tasks;
using OneAI.Native;

internal static class Program
{
    private static async Task<int> Main()
    {
        var done = new TaskCompletionSource<int>();
        var mgr = new EngineProcessManager(new Delegate(done));
        mgr.Start();
        return await done.Task;
    }

    private sealed class Delegate : IEngineProcessManagerDelegate
    {
        private readonly TaskCompletionSource<int> _done;
        public Delegate(TaskCompletionSource<int> done) => _done = done;
        public async Task OnStarted(OneAiRpcClient client)
        {
            try
            {
                var result = await client.Call("scenario/list", null);
                if (result.TryGetProperty("scenarios", out var arr))
                {
                    Console.WriteLine($"scenarios: {arr.GetArrayLength()}");
                    foreach (var s in arr.EnumerateArray())
                        Console.WriteLine("  - " + (s.TryGetProperty("name", out var n) ? n.GetString() : "?"));
                }
                _done.TrySetResult(0);
            }
            catch (Exception e)
            {
                Console.Error.WriteLine($"smoke failed: {e.Message}");
                _done.TrySetResult(1);
            }
        }
        public void OnFailed(Exception error)
        {
            Console.Error.WriteLine($"engine failed: {error.Message}");
            _done.TrySetResult(1);
        }
    }
}
