// ──────────────────────────────────────────────────────────────────────
// OneAI Swift SDK Wrapper
//
// A higher-level Swift API that wraps the raw UniFFI-generated bindings
// for iOS/macOS developers. Provides idiomatic Swift patterns:
// - Result-based error handling
// - Async/await wrappers (via continuation bridging)
// - Swift-native builder pattern
// - Enum convenience extensions
//
// Usage:
//   let app = try OneAI.build {
//       .autoApproval
//       .memory(windowSize: 10, thresholdTokens: 2000)
//       .persistence("/tmp/oneai/checkpoints")
//   }
//   let session = app.createSession()
//   try session.sendMessage("Hello!")
// ──────────────────────────────────────────────────────────────────────

import Foundation

// ─── Error Mapping ────────────────────────────────────────────────────

/// Swift-native error type mapping from UniFFI's flat error enum.
/// Provides structured error handling with typed cases.
enum OneAIError: LocalizedError {
    case provider(String)
    case parser(String)
    case tool(String)
    case memory(String)
    case workflow(String)
    case agent(String)
    case skill(String)
    case scheduler(String)
    case persistence(String)
    case rag(String)
    case config(String)
    case approval(String)
    case serialization(String)
    case network(String)
    case timeout(String)
    case other(String)

    var errorDescription: String? {
        switch self {
        case .provider(let msg):     return "Provider error: \(msg)"
        case .parser(let msg):       return "Parser error: \(msg)"
        case .tool(let msg):         return "Tool error: \(msg)"
        case .memory(let msg):       return "Memory error: \(msg)"
        case .workflow(let msg):     return "Workflow error: \(msg)"
        case .agent(let msg):        return "Agent error: \(msg)"
        case .skill(let msg):        return "Skill error: \(msg)"
        case .scheduler(let msg):    return "Scheduler error: \(msg)"
        case .persistence(let msg):  return "Persistence error: \(msg)"
        case .rag(let msg):          return "RAG error: \(msg)"
        case .config(let msg):       return "Config error: \(msg)"
        case .approval(let msg):     return "Approval error: \(msg)"
        case .serialization(let msg):return "Serialization error: \(msg)"
        case .network(let msg):      return "Network error: \(msg)"
        case .timeout(let msg):      return "Timeout error: \(msg)"
        case .other(let msg):        return "Error: \(msg)"
        }
    }
}

/// Convert UniFFI error view to Swift-native error.
extension OneAIErrorView {
    func toError() -> OneAIError {
        switch self {
        case .Provider(let msg):     return .provider(msg)
        case .Parser(let msg):       return .parser(msg)
        case .Tool(let msg):         return .tool(msg)
        case .Memory(let msg):       return .memory(msg)
        case .Workflow(let msg):     return .workflow(msg)
        case .Agent(let msg):        return .agent(msg)
        case .Skill(let msg):        return .skill(msg)
        case .Scheduler(let msg):    return .scheduler(msg)
        case .Persistence(let msg):  return .persistence(msg)
        case .Rag(let msg):          return .rag(msg)
        case .Config(let msg):       return .config(msg)
        case .Approval(let msg):     return .approval(msg)
        case .Serialization(let msg):return .serialization(msg)
        case .Network(let msg):      return .network(msg)
        case .Timeout(let msg):      return .timeout(msg)
        case .Other(let msg):        return .other(msg)
        }
    }
}

// ─── Builder Configuration ────────────────────────────────────────────

/// Configuration options for building a OneAI App.
/// Used with [OneAI.build] to configure the app declaratively.
struct OneAIConfig {
    var approvalMode: ApprovalMode = .auto
    var memoryWindowSize: UInt32 = 20
    var memoryThresholdTokens: UInt32 = 2000
    var persistencePath: String? = nil
    var useDefaultParser: Bool = true
    var provider: ProviderConfig? = nil

    enum ApprovalMode {
        case auto       // Auto-approve all tool calls
        case blocking   // Block all tool calls (requires manual approval)
    }

    enum ProviderConfig {
        case openAI(apiKey: String, baseUrl: String? = nil, model: String = "gpt-4")
        case anthropic(apiKey: String, model: String = "claude-3-opus")
        case ollama(host: String? = nil, port: UInt16? = nil, model: String = "llama3")
    }
}

// ─── OneAI Builder ────────────────────────────────────────────────────

/// Main entry point for the OneAI Swift SDK.
/// Provides a declarative builder pattern for constructing the app.
enum OneAI {

    /// Build a OneAI App with the given configuration.
    ///
    /// Example:
    /// ```swift
    /// let app = try OneAI.build {
    ///     .autoApproval
    ///     .memory(windowSize: 10, thresholdTokens: 2000)
    ///     .persistence("/tmp/oneai/checkpoints")
    /// }
    /// ```
    ///
    /// - Parameter configure: A closure that modifies [OneAIConfig].
    /// - Returns: A configured [OneAIApp] ready for use.
    /// - Throws: [OneAIError] if the build fails.
    static func build(_ configure: (inout OneAIConfig) -> Void) throws -> OneAIApp {
        var config = OneAIConfig()
        configure(&config)

        var builder = OneAIAppBuilder()

        // Approval gate
        switch config.approvalMode {
        case .auto:    builder = builder.autoApprovalGate()
        case .blocking: builder = builder.blockingApprovalGate()
        }

        // Parser
        if config.useDefaultParser {
            builder = builder.defaultParser()
        }

        // Memory
        builder = builder.memoryManagerWithConfig(
            windowSize: config.memoryWindowSize,
            thresholdTokens: config.memoryThresholdTokens
        )

        // Persistence
        if let path = config.persistencePath {
            builder = builder.persistence(path)
        }

        return try builder.build()
    }

    // ─── Configuration Modifiers ──────────────────────────────────────

    /// Modifier: use auto-approval mode.
    static func autoApproval(_ config: inout OneAIConfig) {
        config.approvalMode = .auto
    }

    /// Modifier: use blocking approval mode.
    static func blockingApproval(_ config: inout OneAIConfig) {
        config.approvalMode = .blocking
    }

    /// Modifier: configure memory parameters.
    static func memory(windowSize: UInt32 = 20, thresholdTokens: UInt32 = 2000,
                       _ config: inout OneAIConfig) {
        config.memoryWindowSize = windowSize
        config.memoryThresholdTokens = thresholdTokens
    }

    /// Modifier: enable file-based persistence.
    static func persistence(_ path: String, _ config: inout OneAIConfig) {
        config.persistencePath = path
    }

    /// Modifier: configure OpenAI provider.
    static func openAI(apiKey: String, baseUrl: String? = nil, model: String = "gpt-4",
                       _ config: inout OneAIConfig) {
        config.provider = .openAI(apiKey: apiKey, baseUrl: baseUrl, model: model)
    }

    /// Modifier: configure Anthropic provider.
    static func anthropic(apiKey: String, model: String = "claude-3-opus",
                          _ config: inout OneAIConfig) {
        config.provider = .anthropic(apiKey: apiKey, model: model)
    }

    /// Modifier: configure Ollama local provider.
    static func ollama(host: String? = nil, port: UInt16? = nil, model: String = "llama3",
                       _ config: inout OneAIConfig) {
        config.provider = .ollama(host: host, port: port, model: model)
    }
}

// ─── Session Extensions ──────────────────────────────────────────────

/// Swift-friendly extensions on the UniFFI-generated [OneAISession].
extension OneAISession {

    /// Send a user message to the session.
    /// - Parameter text: The message text.
    /// - Throws: [OneAIError] on failure.
    func sendMessage(_ text: String) throws {
        try sendUserMessage(text: text)
    }

    /// Execute a tool by name with JSON arguments.
    /// - Parameters:
    ///   - name: The registered tool name.
    ///   - argsJson: JSON-encoded tool arguments.
    /// - Returns: The tool execution result.
    /// - Throws: [OneAIError] on failure.
    func runTool(name: String, argsJson: String) throws -> ToolOutputView {
        return try executeTool(name: name, argsJson: argsJson)
    }

    /// Retrieve relevant memory context.
    /// - Parameters:
    ///   - query: The search query.
    ///   - topK: Maximum number of results (default 5).
    /// - Returns: Joined string of retrieved memory entries.
    /// - Throws: [OneAIError] on failure.
    func queryMemory(_ query: String, topK: UInt32 = 5) throws -> String {
        return try retrieveMemory(query: query, topK: topK)
    }

    /// Save a checkpoint of the current session state.
    /// - Returns: The checkpoint ID.
    /// - Throws: [OneAIError] on failure.
    func checkpoint() throws -> String {
        return try saveCheckpoint()
    }
}

// ─── Tool Factory ────────────────────────────────────────────────────

/// Convenience factory for creating common OneAI tools.
struct OneAITools {
    private static let factory = ToolFactory()

    /// Create a calculator tool.
    static func calculator() -> OneAIToolWrapper {
        return OneAIToolWrapper(inner: factory.createCalculator())
    }

    /// Create a file read tool.
    static func fileReader(maxSizeBytes: UInt64? = nil) -> OneAIToolWrapper {
        return OneAIToolWrapper(inner: factory.createFileReader(maxSizeBytes: maxSizeBytes))
    }

    /// Create a file write tool.
    static func fileWriter() -> OneAIToolWrapper {
        return OneAIToolWrapper(inner: factory.createFileWriter())
    }

    /// Create a shell execution tool.
    static func shell(timeoutSecs: UInt64? = nil) -> OneAIToolWrapper {
        return OneAIToolWrapper(inner:
            timeoutSecs != nil
                ? factory.createShellWithTimeout(timeoutSecs: timeoutSecs!)
                : factory.createShell()
        )
    }
}

// ─── Platform Extensions ──────────────────────────────────────────────

/// Human-readable display name for each platform.
extension PlatformView {
    var displayName: String {
        switch self {
        case .Macos:   return "macOS"
        case .Windows: return "Windows"
        case .Linux:   return "Linux"
        case .Android: return "Android"
        case .Ios:     return "iOS"
        case .Harmony: return "HarmonyOS"
        case .Unknown: return "Unknown"
        }
    }
}

// ─── Approval Response Helpers ────────────────────────────────────────

/// Convenience constructors for approval responses.
extension ApprovalResponseView {
    /// Approve without modifying arguments.
    static func approved() -> ApprovalResponseView {
        return .Approved(modifiedArgsJson: nil)
    }

    /// Approve with modified arguments.
    static func approvedWithModifiedArgs(_ argsJson: String) -> ApprovalResponseView {
        return .Approved(modifiedArgsJson: argsJson)
    }

    /// Deny with a reason.
    static func denied(reason: String) -> ApprovalResponseView {
        return .Denied(reason: reason)
    }

    /// Modify the arguments before approval.
    static func modified(_ argsJson: String) -> ApprovalResponseView {
        return .Modified(argsJson: argsJson)
    }
}

// ─── Risk Level Extensions ────────────────────────────────────────────

/// Human-readable name and approval requirement check.
extension RiskLevelView {
    var displayName: String {
        switch self {
        case .Low:    return "Low"
        case .Medium: return "Medium"
        case .High:   return "High"
        }
    }

    /// Whether this risk level requires human approval before execution.
    var requiresApproval: Bool {
        return self == .High
    }
}