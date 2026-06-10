// ──────────────────────────────────────────────────────────────────────
// OneAI C# SDK Wrapper
//
// A higher-level C# API that wraps the raw UniFFI-generated bindings
// for .NET/desktop developers. Provides idiomatic C# patterns:
// - Exception-based error handling
// - Builder pattern with fluent API
// - async/await wrappers
// - Extension methods on UniFFI types
//
// Usage:
//   var app = OneAI.Build(config => config
//       .AutoApproval()
//       .Memory(windowSize: 10, thresholdTokens: 2000)
//       .Persistence("C:\\oneai\\checkpoints"));
//   var session = app.CreateSession();
//   session.SendMessage("Hello!");
// ──────────────────────────────────────────────────────────────────────

using System;
using System.Threading.Tasks;

// UniFFI-generated bindings namespace (after generate_bindings.sh)
// using OneAI.Uniffi;

namespace OneAI.Sdk
{
    // ─── Exception Mapping ──────────────────────────────────────────────

    /// <summary>
    /// Base exception for all OneAI operations.
    /// Maps from the flat UniFFI error view to typed C# exceptions.
    /// </summary>
    public class OneAIException : Exception
    {
        public OneAIErrorCategory Category { get; }

        public OneAIException(OneAIErrorCategory category, string message)
            : base($"{category}: {message}")
        {
            Category = category;
        }
    }

    /// <summary>
    /// Error category matching the UniFFI error enum variants.
    /// </summary>
    public enum OneAIErrorCategory
    {
        Provider,
        Parser,
        Tool,
        Memory,
        Workflow,
        Agent,
        Skill,
        Scheduler,
        Persistence,
        Rag,
        Config,
        Approval,
        Serialization,
        Network,
        Timeout,
        Other
    }

    /// <summary>
    /// Typed exception subclasses for specific error categories.
    /// </summary>
    public class OneAIProviderException : OneAIException
    {
        public OneAIProviderException(string message) : base(OneAIErrorCategory.Provider, message) { }
    }

    public class OneAIToolException : OneAIException
    {
        public OneAIToolException(string message) : base(OneAIErrorCategory.Tool, message) { }
    }

    public class OneAIMemoryException : OneAIException
    {
        public OneAIMemoryException(string message) : base(OneAIErrorCategory.Memory, message) { }
    }

    public class OneAIWorkflowException : OneAIException
    {
        public OneAIWorkflowException(string message) : base(OneAIErrorCategory.Workflow, message) { }
    }

    public class OneAIApprovalException : OneAIException
    {
        public OneAIApprovalException(string message) : base(OneAIErrorCategory.Approval, message) { }
    }

    // ─── Builder Configuration ──────────────────────────────────────────

    /// <summary>
    /// Configuration class for building a OneAI App.
    /// Used with the fluent builder pattern.
    /// </summary>
    public class OneAIConfig
    {
        internal ApprovalMode ApprovalModeValue { get; set; } = ApprovalMode.Auto;
        internal uint MemoryWindowSize { get; set; } = 20;
        internal uint MemoryThresholdTokens { get; set; } = 2000;
        internal string? PersistencePath { get; set; } = null;
        internal bool UseDefaultParser { get; set; } = true;

        /// <summary>
        /// Approval mode: Auto (approve all) or Blocking (deny all, manual review needed).
        /// </summary>
        public enum ApprovalMode
        {
            Auto,
            Blocking
        }

        // ─── Fluent Modifiers ────────────────────────────────────────

        /// Use auto-approval mode (all tool calls approved automatically).
        public OneAIConfig AutoApproval()
        {
            ApprovalModeValue = ApprovalMode.Auto;
            return this;
        }

        /// Use blocking approval mode (all tool calls denied, manual review needed).
        public OneAIConfig BlockingApproval()
        {
            ApprovalModeValue = ApprovalMode.Blocking;
            return this;
        }

        /// Configure memory manager parameters.
        public OneAIConfig Memory(uint windowSize = 20, uint thresholdTokens = 2000)
        {
            MemoryWindowSize = windowSize;
            MemoryThresholdTokens = thresholdTokens;
            return this;
        }

        /// Enable file-based persistence at the specified path.
        public OneAIConfig Persistence(string path)
        {
            PersistencePath = path;
            return this;
        }

        /// Use the default 3-layer parser.
        public OneAIConfig DefaultParser()
        {
            UseDefaultParser = true;
            return this;
        }
    }

    // ─── OneAI Builder ──────────────────────────────────────────────────

    /// <summary>
    /// Main entry point for the OneAI C# SDK.
    /// Provides a fluent builder pattern for constructing the app.
    /// </summary>
    public static class OneAI
    {
        /// <summary>
        /// Build a OneAI App with the given configuration.
        ///
        /// Example:
        /// <code>
        /// var app = OneAI.Build(config => config
        ///     .AutoApproval()
        ///     .Memory(windowSize: 10, thresholdTokens: 2000)
        ///     .Persistence("C:\\oneai\\checkpoints"));
        /// </code>
        /// </summary>
        public static OneAIApp Build(Action<OneAIConfig> configure)
        {
            var config = new OneAIConfig();
            configure(config);

            var builder = new OneAIAppBuilder();

            // Approval gate
            builder = config.ApprovalModeValue == OneAIConfig.ApprovalMode.Auto
                ? builder.AutoApprovalGate()
                : builder.BlockingApprovalGate();

            // Parser
            if (config.UseDefaultParser)
            {
                builder = builder.DefaultParser();
            }

            // Memory
            builder = builder.MemoryManagerWithConfig(
                windowSize: config.MemoryWindowSize,
                thresholdTokens: config.MemoryThresholdTokens
            );

            // Persistence
            if (config.PersistencePath != null)
            {
                builder = builder.Persistence(config.PersistencePath);
            }

            return builder.Build();
        }

        /// <summary>
        /// Build with default configuration (auto-approval, default parser).
        /// </summary>
        public static OneAIApp BuildDefault()
        {
            return Build(config => config.AutoApproval());
        }
    }

    // ─── Session Extensions ──────────────────────────────────────────────

    /// <summary>
    /// C#-friendly extension methods on the UniFFI-generated OneAISession.
    /// </summary>
    public static class SessionExtensions
    {
        /// <summary>
        /// Send a user message to the session.
        /// </summary>
        public static void SendMessage(this OneAISession session, string text)
        {
            session.SendUserMessage(text);
        }

        /// <summary>
        /// Execute a tool by name with JSON arguments.
        /// Returns the tool execution result.
        /// </summary>
        public static ToolOutputView RunTool(this OneAISession session, string name, string argsJson)
        {
            return session.ExecuteTool(name, argsJson);
        }

        /// <summary>
        /// Retrieve relevant memory context for a query.
        /// </summary>
        public static string QueryMemory(this OneAISession session, string query, uint topK = 5)
        {
            return session.RetrieveMemory(query, topK);
        }

        /// <summary>
        /// Save a checkpoint of the current session state.
        /// </summary>
        public static string Checkpoint(this OneAISession session)
        {
            return session.SaveCheckpoint();
        }
    }

    // ─── Tool Factory ────────────────────────────────────────────────────

    /// <summary>
    /// Convenience factory for creating common OneAI tools.
    /// </summary>
    public static class OneAITools
    {
        private static readonly ToolFactory _factory = new ToolFactory();

        /// Create a calculator tool.
        public static OneAIToolWrapper Calculator()
        {
            return new OneAIToolWrapper(_factory.CreateCalculator());
        }

        /// Create a file read tool.
        public static OneAIToolWrapper FileReader(ulong? maxSizeBytes = null)
        {
            return new OneAIToolWrapper(_factory.CreateFileReader(maxSizeBytes));
        }

        /// Create a file write tool.
        public static OneAIToolWrapper FileWriter()
        {
            return new OneAIToolWrapper(_factory.CreateFileWriter());
        }

        /// Create a shell execution tool.
        public static OneAIToolWrapper Shell(ulong? timeoutSecs = null)
        {
            return new OneAIToolWrapper(
                timeoutSecs != null
                    ? _factory.CreateShellWithTimeout(timeoutSecs.Value)
                    : _factory.CreateShell()
            );
        }
    }

    // ─── Platform Extensions ────────────────────────────────────────────

    /// <summary>
    /// Human-readable display name for each platform.
    /// </summary>
    public static class PlatformExtensions
    {
        public static string DisplayName(this PlatformView platform)
        {
            return platform switch
            {
                PlatformView.Macos => "macOS",
                PlatformView.Windows => "Windows",
                PlatformView.Linux => "Linux",
                PlatformView.Android => "Android",
                PlatformView.Ios => "iOS",
                PlatformView.Harmony => "HarmonyOS",
                PlatformView.Unknown => "Unknown",
                _ => "Unknown"
            };
        }
    }

    // ─── Approval Response Helpers ──────────────────────────────────────

    /// <summary>
    /// Convenience constructors for approval responses.
    /// </summary>
    public static class ApprovalResponses
    {
        /// Approve without modifying arguments.
        public static ApprovalResponseView Approved()
        {
            return ApprovalResponseView.Approved(null);
        }

        /// Approve with modified arguments.
        public static ApprovalResponseView ApprovedWithModifiedArgs(string argsJson)
        {
            return ApprovalResponseView.Approved(argsJson);
        }

        /// Deny with a reason.
        public static ApprovalResponseView Denied(string reason)
        {
            return ApprovalResponseView.Denied(reason);
        }

        /// Modify the arguments before approval.
        public static ApprovalResponseView Modified(string argsJson)
        {
            return ApprovalResponseView.Modified(argsJson);
        }
    }

    // ─── Risk Level Extensions ──────────────────────────────────────────

    /// <summary>
    /// Human-readable name and approval requirement check.
    /// </summary>
    public static class RiskLevelExtensions
    {
        public static string DisplayName(this RiskLevelView level)
        {
            return level switch
            {
                RiskLevelView.Low => "Low",
                RiskLevelView.Medium => "Medium",
                RiskLevelView.High => "High",
                _ => "Unknown"
            };
        }

        /// Whether this risk level requires human approval before execution.
        public static bool RequiresApproval(this RiskLevelView level)
        {
            return level == RiskLevelView.High;
        }
    }
}