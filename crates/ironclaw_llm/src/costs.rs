//! Per-model cost lookup table for multi-provider LLM support.
//!
//! Returns (input_cost_per_token, output_cost_per_token) as Decimal pairs.
//! Ollama and other local models return zero cost.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Look up known per-token costs for a model by its identifier.
///
/// Returns `Some((input_cost, output_cost))` for known models, `None` otherwise.
pub fn model_cost(model_id: &str) -> Option<(Decimal, Decimal)> {
    // OpenRouter free-tier models: `:free` suffix or the `openrouter/free` router
    // should always report zero cost (see #463).
    if model_id.ends_with(":free") || model_id == "openrouter/free" || model_id == "free" {
        return Some((Decimal::ZERO, Decimal::ZERO));
    }

    // Normalize: strip provider prefixes (e.g., "openai/gpt-4o" -> "gpt-4o")
    let id = model_id
        .rsplit_once('/')
        .map(|(_, name)| name)
        .unwrap_or(model_id);

    match id {
        // OpenAI models -- prices per token (USD)
        // GPT-4o, GPT-4.1, o4-mini retired 2026-02-13
        "gpt-5.3-codex" => Some((dec!(0.0000025), dec!(0.000012))),
        "gpt-5.2" => Some((dec!(0.00000175), dec!(0.000014))),
        "gpt-5-mini" => Some((dec!(0.00000025), dec!(0.000002))),
        "gpt-5-nano" => Some((dec!(0.00000005), dec!(0.0000004))),
        "o3-mini" | "o3-mini-2025-01-31" => Some((dec!(0.0000011), dec!(0.0000044))),

        // Anthropic models
        "claude-opus-4.6" | "claude-opus-4-6-20260205" => {
            Some((dec!(0.000005), dec!(0.000025)))
        }
        "claude-sonnet-4" | "claude-sonnet-4-20250514" => {
            Some((dec!(0.000003), dec!(0.000015)))
        }
        "claude-haiku-4.5" | "claude-haiku-4-5-20241022" => {
            Some((dec!(0.000001), dec!(0.000005)))
        }
        // Legacy Anthropic (still in some configs)
        "claude-3-5-sonnet-20241022" | "claude-3-5-sonnet-latest" => {
            Some((dec!(0.000003), dec!(0.000015)))
        }
        "claude-3-opus-20240229" | "claude-3-opus-latest" | "claude-opus-4-20250514" => {
            Some((dec!(0.000015), dec!(0.000075)))
        }

        // Google Gemini models
        "gemini-3-pro-preview" | "gemini-3-pro" => {
            Some((dec!(0.000002), dec!(0.000012)))
        }
        "gemini-2.5-pro" | "gemini-2.5-pro-latest" => {
            Some((dec!(0.00000125), dec!(0.00001)))
        }
        "gemini-2.5-flash" | "gemini-2.5-flash-latest" => {
            Some((dec!(0.00000015), dec!(0.0000006)))
        }
        "gemini-3-flash-preview" | "gemini-3-flash" => {
            Some((dec!(0.00000015), dec!(0.0000006)))
        }

        // Ollama / local models -- free
        _ if is_local_model(id) => Some((Decimal::ZERO, Decimal::ZERO)),

        _ => None,
    }
}

/// Default cost for unknown models.
pub fn default_cost() -> (Decimal, Decimal) {
    // Conservative estimate: roughly GPT-5 Mini pricing
    (dec!(0.00000025), dec!(0.000002))
}

/// Heuristic to detect local/self-hosted models (Ollama, llama.cpp, etc.).
fn is_local_model(model_id: &str) -> bool {
    let lower = model_id.to_lowercase();
    lower.starts_with("llama")
        || lower.starts_with("mistral")
        || lower.starts_with("mixtral")
        || lower.starts_with("phi")
        || lower.starts_with("gemma")
        || lower.starts_with("qwen")
        || lower.starts_with("codellama")
        || lower.starts_with("codestral")
        || lower.starts_with("deepseek")
        || lower.starts_with("deepcoder")
        || lower.starts_with("starcoder")
        || lower.starts_with("vicuna")
        || lower.starts_with("yi")
        || lower.starts_with("snowflake")
        || lower.contains(":latest")
        || lower.contains(":instruct")
        || lower.contains(":3b")
        || lower.contains(":7b")
        || lower.contains(":14b")
        || lower.contains(":22b")
        || lower.contains(":27b")
        || lower.contains(":30b")
        || lower.contains(":32b")
        || lower.contains(":70b")
        || lower.contains(":72b")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_model_costs() {
        let (input, output) = model_cost("gpt-5.3-codex").unwrap();
        assert!(input > Decimal::ZERO);
        assert!(output > input);
    }

    #[test]
    fn test_claude_costs() {
        let (input, output) = model_cost("claude-opus-4.6").unwrap();
        assert!(input > Decimal::ZERO);
        assert!(output > input);
    }

    #[test]
    fn test_gemini_costs() {
        let (input, output) = model_cost("gemini-3-pro-preview").unwrap();
        assert!(input > Decimal::ZERO);
        assert!(output > input);
    }

    #[test]
    fn test_local_model_free() {
        let (input, output) = model_cost("qwen3-coder:30b").unwrap();
        assert_eq!(input, Decimal::ZERO);
        assert_eq!(output, Decimal::ZERO);
    }

    #[test]
    fn test_ollama_tagged_model_free() {
        let (input, output) = model_cost("deepseek-r1:70b").unwrap();
        assert_eq!(input, Decimal::ZERO);
        assert_eq!(output, Decimal::ZERO);
    }

    #[test]
    fn test_unknown_model_returns_none() {
        assert!(model_cost("some-totally-unknown-model-xyz").is_none());
    }

    #[test]
    fn test_default_cost_nonzero() {
        let (input, output) = default_cost();
        assert!(input > Decimal::ZERO);
        assert!(output > Decimal::ZERO);
    }

    #[test]
    fn test_provider_prefix_stripped() {
        assert_eq!(model_cost("openai/gpt-5.2"), model_cost("gpt-5.2"));
    }

    #[test]
    fn test_openrouter_free_suffix_zero_cost() {
        // Models with `:free` suffix should report zero cost (#463)
        let (input, output) = model_cost("stepfun/step-3.5-flash:free").unwrap();
        assert_eq!(input, Decimal::ZERO);
        assert_eq!(output, Decimal::ZERO);
    }

    #[test]
    fn test_openrouter_free_router_zero_cost() {
        // The "openrouter/free" router model should report zero cost (#463)
        let (input, output) = model_cost("openrouter/free").unwrap();
        assert_eq!(input, Decimal::ZERO);
        assert_eq!(output, Decimal::ZERO);
    }

    #[test]
    fn test_bare_free_zero_cost() {
        // Edge case: bare "free" after prefix stripping
        let (input, output) = model_cost("free").unwrap();
        assert_eq!(input, Decimal::ZERO);
        assert_eq!(output, Decimal::ZERO);
    }

    #[test]
    fn test_free_suffix_various_providers() {
        // Various provider-prefixed free models
        for model in &[
            "google/gemma-3-27b-it:free",
            "meta-llama/llama-4-maverick:free",
            "microsoft/phi-4:free",
            "nousresearch/deephermes-3-llama-3-8b-preview:free",
        ] {
            let (input, output) =
                model_cost(model).unwrap_or_else(|| panic!("{model} should return Some"));
            assert_eq!(input, Decimal::ZERO, "{model} input cost should be zero");
            assert_eq!(output, Decimal::ZERO, "{model} output cost should be zero");
        }
    }
}
