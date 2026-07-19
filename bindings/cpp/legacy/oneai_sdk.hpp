// ──────────────────────────────────────────────────────────────────────
// OneAI C++ SDK Wrapper — Header
//
// A higher-level C++ API that wraps the raw UniFFI-generated bindings
// for C++ developers. Provides idiomatic C++ patterns:
// - Exception-based error handling (std::runtime_error hierarchy)
// - RAII builder pattern
// - Convenience factory functions
// - Extension-style helper methods
//
// Usage:
//   auto app = oneai::Builder()
//       .auto_approval()
//       .memory(10, 2000)
//       .persistence("/tmp/oneai/checkpoints")
//       .build();
//   auto session = app.create_session();
//   session.send_message("Hello!");
// ──────────────────────────────────────────────────────────────────────

#pragma once

#include <string>
#include <memory>
#include <optional>
#include <stdexcept>
#include <functional>
#include <vector>

// UniFFI-generated C++ bindings (after generate_bindings.sh)
// #include "oneai.h"

namespace oneai {

// ─── Exception Hierarchy ──────────────────────────────────────────────

/// Base exception for all OneAI C++ SDK errors.
class Exception : public std::runtime_error {
public:
    explicit Exception(const std::string& msg)
        : std::runtime_error(msg) {}
};

/// Provider-related errors (LLM API failures).
class ProviderException : public Exception {
public:
    explicit ProviderException(const std::string& msg)
        : Exception("Provider: " + msg) {}
};

/// Tool execution errors.
class ToolException : public Exception {
public:
    explicit ToolException(const std::string& msg)
        : Exception("Tool: " + msg) {}
};

/// Memory operation errors.
class MemoryException : public Exception {
public:
    explicit MemoryException(const std::string& msg)
        : Exception("Memory: " + msg) {}
};

/// Workflow execution errors.
class WorkflowException : public Exception {
public:
    explicit WorkflowException(const std::string& msg)
        : Exception("Workflow: " + msg) {}
};

/// Approval gate errors.
class ApprovalException : public Exception {
public:
    explicit ApprovalException(const std::string& msg)
        : Exception("Approval: " + msg) {}
};

/// Parser errors.
class ParserException : public Exception {
public:
    explicit ParserException(const std::string& msg)
        : Exception("Parser: " + msg) {}
};

// ─── Builder ──────────────────────────────────────────────────────────

/// Configuration for building a OneAI App.
struct BuilderConfig {
    enum ApprovalMode { Auto, Blocking };
    ApprovalMode approval = Auto;
    uint32_t memory_window_size = 20;
    uint32_t memory_threshold_tokens = 2000;
    std::optional<std::string> persistence_path;
    bool use_default_parser = true;
};

/// Fluent builder for constructing a OneAI App.
///
/// Example:
/// ```cpp
/// auto app = oneai::Builder()
///     .auto_approval()
///     .memory(10, 2000)
///     .persistence("/tmp/oneai/checkpoints")
///     .build();
/// ```
class Builder {
public:
    Builder() = default;

    /// Use auto-approval mode (all tool calls approved automatically).
    Builder& auto_approval() {
        config_.approval = BuilderConfig::Auto;
        return *this;
    }

    /// Use blocking approval mode (all tool calls denied).
    Builder& blocking_approval() {
        config_.approval = BuilderConfig::Blocking;
        return *this;
    }

    /// Configure memory manager parameters.
    Builder& memory(uint32_t window_size = 20, uint32_t threshold_tokens = 2000) {
        config_.memory_window_size = window_size;
        config_.memory_threshold_tokens = threshold_tokens;
        return *this;
    }

    /// Enable file-based persistence at the specified path.
    Builder& persistence(const std::string& path) {
        config_.persistence_path = path;
        return *this;
    }

    /// Use the default 3-layer parser.
    Builder& default_parser() {
        config_.use_default_parser = true;
        return *this;
    }

    /// Build the application from the accumulated configuration.
    ///
    /// @throws Exception if the build fails.
    OneAIApp build() {
        auto builder = OneAIAppBuilder();

        // Approval gate
        switch (config_.approval) {
            case BuilderConfig::Auto:
                builder = builder.auto_approval_gate();
                break;
            case BuilderConfig::Blocking:
                builder = builder.blocking_approval_gate();
                break;
        }

        // Parser
        if (config_.use_default_parser) {
            builder = builder.default_parser();
        }

        // Memory
        builder = builder.memory_manager_with_config(
            config_.memory_window_size,
            config_.memory_threshold_tokens
        );

        // Persistence
        if (config_.persistence_path.has_value()) {
            builder = builder.persistence(config_.persistence_path.value());
        }

        try {
            return builder.build();
        } catch (const OneAIErrorView& err) {
            throw map_error(err);
        }
    }

private:
    BuilderConfig config_;

    /// Map UniFFI error view to C++ exception hierarchy.
    static Exception map_error(const OneAIErrorView& err) {
        // The UniFFI-generated error is a flat enum; map to typed exceptions
        // Actual variant matching depends on the generated C++ binding structure
        return Exception("OneAI build error");
    }
};

// ─── Session Helpers ──────────────────────────────────────────────────

/// C++-friendly wrapper around the UniFFI-generated OneAISession.
/// Provides method renaming and exception translation.
class Session {
public:
    explicit Session(OneAISession inner) : inner_(std::move(inner)) {}

    /// Get the session ID.
    std::string session_id() const {
        return inner_.session_id();
    }

    /// Send a user message to the session.
    void send_message(const std::string& text) {
        inner_.send_user_message(text);
    }

    /// Execute a tool by name with JSON arguments.
    ToolOutputView execute_tool(const std::string& name, const std::string& args_json) {
        return inner_.execute_tool(name, args_json);
    }

    /// Retrieve relevant memory context.
    std::string query_memory(const std::string& query, uint32_t top_k = 5) {
        return inner_.retrieve_memory(query, top_k);
    }

private:
    OneAISession inner_;
};

// ─── Tool Factory ────────────────────────────────────────────────────

/// Convenience factory for creating common OneAI tools.
namespace tools {

inline OneAIToolWrapper calculator() {
    static ToolFactory factory;
    return OneAIToolWrapper(factory.create_calculator());
}

inline OneAIToolWrapper file_reader(std::optional<uint64_t> max_size_bytes = std::nullopt) {
    static ToolFactory factory;
    return OneAIToolWrapper(factory.create_file_reader(max_size_bytes));
}

inline OneAIToolWrapper file_writer() {
    static ToolFactory factory;
    return OneAIToolWrapper(factory.create_file_writer());
}

inline OneAIToolWrapper shell(std::optional<uint64_t> timeout_secs = std::nullopt) {
    static ToolFactory factory;
    if (timeout_secs.has_value()) {
        return OneAIToolWrapper(factory.create_shell_with_timeout(timeout_secs.value()));
    }
    return OneAIToolWrapper(factory.create_shell());
}

} // namespace tools

// ─── Platform Display Names ───────────────────────────────────────────

/// Get human-readable platform display name.
inline std::string platform_display_name(PlatformView platform) {
    switch (platform) {
        case PlatformView::Macos:   return "macOS";
        case PlatformView::Windows: return "Windows";
        case PlatformView::Linux:   return "Linux";
        case PlatformView::Android: return "Android";
        case PlatformView::Ios:     return "iOS";
        case PlatformView::Harmony: return "HarmonyOS";
        case PlatformView::Unknown: return "Unknown";
        default:                    return "Unknown";
    }
}

// ─── Approval Response Helpers ────────────────────────────────────────

/// Create approval responses with convenience functions.
namespace approval {

/// Approve without modifying arguments.
inline ApprovalResponseView approved() {
    return ApprovalResponseView::Approved(std::nullopt);
}

/// Approve with modified arguments.
inline ApprovalResponseView approved_with_args(const std::string& args_json) {
    return ApprovalResponseView::Approved(args_json);
}

/// Deny with a reason.
inline ApprovalResponseView denied(const std::string& reason) {
    return ApprovalResponseView::Denied(reason);
}

/// Modify the arguments before approval.
inline ApprovalResponseView modified(const std::string& args_json) {
    return ApprovalResponseView::Modified(args_json);
}

} // namespace approval

// ─── Risk Level Helpers ──────────────────────────────────────────────

/// Get human-readable risk level name.
inline std::string risk_level_display_name(RiskLevelView level) {
    switch (level) {
        case RiskLevelView::Low:    return "Low";
        case RiskLevelView::Medium: return "Medium";
        case RiskLevelView::High:   return "High";
        default:                    return "Unknown";
    }
}

/// Check if a risk level requires human approval.
inline bool risk_requires_approval(RiskLevelView level) {
    return level == RiskLevelView::High;
}

} // namespace oneai