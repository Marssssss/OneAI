// ──────────────────────────────────────────────────────────────────────
// OneAI Kotlin SDK Wrapper
//
// A higher-level Kotlin API that wraps the raw UniFFI-generated bindings
// for Android developers. Provides idiomatic Kotlin patterns:
// - Extension functions on UniFFI types
// - Kotlin-friendly builders with DSL-style configuration
// - Coroutine-based async wrappers (via CompletableFuture → Deferred)
// - Sealed-class error mapping
//
// Usage:
//   val app = OneAI.build {
//       autoApproval()
//       memory(windowSize = 10, thresholdTokens = 2000)
//       persistence("/data/oneai/checkpoints")
//   }
//   val session = app.createSession()
//   session.sendMessage("Hello!")
// ──────────────────────────────────────────────────────────────────────

package ai.oneai.sdk

// ─── Imports ──────────────────────────────────────────────────────────
// These import from the UniFFI-generated module (oneai.uniffi)
// After running generate_bindings.sh, the generated module lives under:
//   bindings/kotlin/ai/oneai/uniffi/

import ai.oneai.uniffi.*

// ─── OneAI Error Mapping ─────────────────────────────────────────────

/**
 * Sealed class representing OneAI errors in Kotlin-friendly form.
 * Maps from the flat [OneAIErrorView] enum to Kotlin exception hierarchy.
 */
sealed class OneAIException(message: String) : Exception(message) {
    class Provider(msg: String) : OneAIException(msg)
    class Parser(msg: String) : OneAIException(msg)
    class Tool(msg: String) : OneAIException(msg)
    class Memory(msg: String) : OneAIException(msg)
    class Workflow(msg: String) : OneAIException(msg)
    class Agent(msg: String) : OneAIException(msg)
    class Skill(msg: String) : OneAIException(msg)
    class Scheduler(msg: String) : OneAIException(msg)
    class Persistence(msg: String) : OneAIException(msg)
    class Rag(msg: String) : OneAIException(msg)
    class Config(msg: String) : OneAIException(msg)
    class Approval(msg: String) : OneAIException(msg)
    class Serialization(msg: String) : OneAIException(msg)
    class Network(msg: String) : OneAIException(msg)
    class Timeout(msg: String) : OneAIException(msg)
    class Other(msg: String) : OneAIException(msg)
}

/** Convert UniFFI error view to Kotlin sealed exception. */
fun OneAIErrorView.toException(): OneAIException = when (this) {
    is OneAIErrorView.Provider -> OneAIException.Provider(message)
    is OneAIErrorView.Parser -> OneAIException.Parser(message)
    is OneAIErrorView.Tool -> OneAIException.Tool(message)
    is OneAIErrorView.Memory -> OneAIException.Memory(message)
    is OneAIErrorView.Workflow -> OneAIException.Workflow(message)
    is OneAIErrorView.Agent -> OneAIException.Agent(message)
    is OneAIErrorView.Skill -> OneAIException.Skill(message)
    is OneAIErrorView.Scheduler -> OneAIException.Scheduler(message)
    is OneAIErrorView.Persistence -> OneAIException.Persistence(message)
    is OneAIErrorView.Rag -> OneAIException.Rag(message)
    is OneAIErrorView.Config -> OneAIException.Config(message)
    is OneAIErrorView.Approval -> OneAIException.Approval(message)
    is OneAIErrorView.Serialization -> OneAIException.Serialization(message)
    is OneAIErrorView.Network -> OneAIException.Network(message)
    is OneAIErrorView.Timeout -> OneAIException.Timeout(message)
    is OneAIErrorView.Other -> OneAIException.Other(message)
}

// ─── Builder DSL ──────────────────────────────────────────────────────

/**
 * Configuration for the OneAI App builder.
 * Used inside [OneAI.build] to configure the app before construction.
 */
class OneAIConfig {
    var approvalMode: ApprovalMode = ApprovalMode.Auto
    var memoryWindowSize: UInt = 20u
    var memoryThresholdTokens: UInt = 2000u
    var persistencePath: String? = null
    var useDefaultParser: Boolean = true

    /** Provider configuration (set separately). */
    var provider: ProviderConfig? = null

    enum class ApprovalMode {
        Auto,    // Auto-approve all tool calls
        Blocking // Block all tool calls (requires manual approval)
    }

    sealed class ProviderConfig {
        data class OpenAI(
            val apiKey: String,
            val baseUrl: String? = null,
            val model: String = "gpt-4"
        ) : ProviderConfig()

        data class Anthropic(
            val apiKey: String,
            val model: String = "claude-3-opus"
        ) : ProviderConfig()

        data class Ollama(
            val host: String? = null,
            val port: UInt? = null,
            val model: String = "llama3"
        ) : ProviderConfig()
    }
}

/**
 * DSL-style builder for constructing a OneAI App.
 *
 * Example:
 * ```
 * val app = OneAI.build {
 *     autoApproval()
 *     memory(windowSize = 10, thresholdTokens = 2000)
 *     persistence("/data/oneai/checkpoints")
 *     openAI(apiKey = "sk-...", model = "gpt-4")
 * }
 * ```
 */
object OneAI {

    fun build(configure: OneAIConfig.() -> Unit): OneAIApp {
        val config = OneAIConfig().apply(configure)

        var builder = OneAIAppBuilder()

        // Approval gate
        builder = when (config.approvalMode) {
            OneAIConfig.ApprovalMode.Auto -> builder.autoApprovalGate()
            OneAIConfig.ApprovalMode.Blocking -> builder.blockingApprovalGate()
        }

        // Parser
        if (config.useDefaultParser) {
            builder = builder.defaultParser()
        }

        // Memory
        builder = builder.memoryManagerWithConfig(
            windowSize = config.memoryWindowSize,
            thresholdTokens = config.memoryThresholdTokens
        )

        // Persistence
        config.persistencePath?.let { path ->
            builder = builder.persistence(path)
        }

        return builder.build()
    }

    // ─── Convenience DSL Functions ────────────────────────────────────

    /** Set auto-approval mode. */
    fun OneAIConfig.autoApproval() {
        approvalMode = OneAIConfig.ApprovalMode.Auto
    }

    /** Set blocking (manual approval) mode. */
    fun OneAIConfig.blockingApproval() {
        approvalMode = OneAIConfig.ApprovalMode.Blocking
    }

    /** Configure memory with custom parameters. */
    fun OneAIConfig.memory(windowSize: UInt = 20u, thresholdTokens: UInt = 2000u) {
        memoryWindowSize = windowSize
        memoryThresholdTokens = thresholdTokens
    }

    /** Enable file-based persistence at the given path. */
    fun OneAIConfig.persistence(path: String) {
        persistencePath = path
    }

    /** Configure OpenAI provider. */
    fun OneAIConfig.openAI(apiKey: String, baseUrl: String? = null, model: String = "gpt-4") {
        provider = OneAIConfig.ProviderConfig.OpenAI(apiKey, baseUrl, model)
    }

    /** Configure Anthropic provider. */
    fun OneAIConfig.anthropic(apiKey: String, model: String = "claude-3-opus") {
        provider = OneAIConfig.ProviderConfig.Anthropic(apiKey, model)
    }

    /** Configure Ollama local provider. */
    fun OneAIConfig.ollama(host: String? = null, port: UInt? = null, model: String = "llama3") {
        provider = OneAIConfig.ProviderConfig.Ollama(host, port, model)
    }
}

// ─── Session Extensions ──────────────────────────────────────────────

/**
 * Kotlin-friendly extension methods on the UniFFI-generated [OneAISession].
 * Wraps the low-level API with idiomatic Kotlin patterns.
 */

/** Send a user message (synchronous wrapper for convenience). */
fun OneAISession.sendMessage(text: String): Result<Unit> {
    return try {
        sendUserMessage(text)
        Result.success(Unit)
    } catch (e: Exception) {
        Result.failure(e)
    }
}

/** Execute a tool with JSON arguments. */
fun OneAISession.executeTool(name: String, argsJson: String): Result<ToolOutputView> {
    return try {
        Result.success(executeTool(name, argsJson))
    } catch (e: Exception) {
        Result.failure(e)
    }
}

/** Retrieve memory context as a joined string. */
fun OneAISession.queryMemory(query: String, topK: UInt = 5u): Result<String> {
    return try {
        Result.success(retrieveMemory(query, topK))
    } catch (e: Exception) {
        Result.failure(e)
    }
}

/** Save a checkpoint. */
fun OneAISession.saveCheckpoint(): Result<String> {
    return try {
        Result.success(saveCheckpoint())
    } catch (e: Exception) {
        Result.failure(e)
    }
}

// ─── Tool Factory Helpers ─────────────────────────────────────────────

/**
 * Convenience factory for creating common tools.
 * Wraps [ToolFactory] with Kotlin-friendly naming.
 */
object Tools {
    private val factory = ToolFactory()

    fun calculator(): OneAIToolWrapper {
        // CalculatorTool is a concrete type that can be used directly
        // The OneAIToolWrapper wraps Arc<dyn Tool> for UniFFI
        return OneAIToolWrapper(factory.createCalculator())
    }

    fun fileReader(maxSizeBytes: UInt? = null): OneAIToolWrapper {
        return OneAIToolWrapper(factory.createFileReader(maxSizeBytes))
    }

    fun fileWriter(): OneAIToolWrapper {
        return OneAIToolWrapper(factory.createFileWriter())
    }

    fun shell(timeoutSecs: UInt? = null): OneAIToolWrapper {
        return OneAIToolWrapper(
            if (timeoutSecs != null) factory.createShellWithTimeout(timeoutSecs)
            else factory.createShell()
        )
    }
}

// ─── Platform Detection ──────────────────────────────────────────────

/** Get the detected platform as a Kotlin enum. */
fun OneAIApp.platform(): PlatformView = this.platform()

/** Human-readable platform name. */
fun PlatformView.displayName(): String = when (this) {
    PlatformView.MACOS -> "macOS"
    PlatformView.WINDOWS -> "Windows"
    PlatformView.LINUX -> "Linux"
    PlatformView.ANDROID -> "Android"
    PlatformView.IOS -> "iOS"
    PlatformView.HARMONY -> "HarmonyOS"
    PlatformView.UNKNOWN -> "Unknown"
}

// ─── Approval Response Helpers ──────────────────────────────────────

/** Create an approved response (no argument modification). */
fun ApprovalResponseView.Companion.approved(): ApprovalResponseView {
    return ApprovalResponseView.Approved(modifiedArgsJson = null)
}

/** Create an approved response with modified arguments. */
fun ApprovalResponseView.Companion.approvedWithModifiedArgs(argsJson: String): ApprovalResponseView {
    return ApprovalResponseView.Approved(modifiedArgsJson = argsJson)
}

/** Create a denied response with a reason. */
fun ApprovalResponseView.Companion.denied(reason: String): ApprovalResponseView {
    return ApprovalResponseView.Denied(reason = reason)
}

/** Create a modified response with new arguments. */
fun ApprovalResponseView.Companion.modified(argsJson: String): ApprovalResponseView {
    return ApprovalResponseView.Modified(argsJson = argsJson)
}

// ─── Risk Level Helpers ──────────────────────────────────────────────

/** Human-readable risk level name. */
fun RiskLevelView.displayName(): String = when (this) {
    RiskLevelView.LOW -> "Low"
    RiskLevelView.MEDIUM -> "Medium"
    RiskLevelView.HIGH -> "High"
}

/** Check if a risk level requires human approval. */
fun RiskLevelView.requiresApproval(): Boolean = this == RiskLevelView.HIGH