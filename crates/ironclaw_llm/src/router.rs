//! Intelligent LLM request router.
//!
//! Classifies incoming requests by complexity using a weighted 15-dimension
//! scoring system (ported from ClawRouter) and routes to the optimal model
//! for cost/quality balance. Supports 4 routing profiles: auto, eco, premium, free.
//!
//! The classification is a pure function running in <1ms with no external calls.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use std::sync::LazyLock as Lazy;
use regex::Regex;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Complexity tiers
// ---------------------------------------------------------------------------

/// The four complexity tiers a request can be classified into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Tier {
    Simple,
    Medium,
    Complex,
    Reasoning,
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tier::Simple => write!(f, "SIMPLE"),
            Tier::Medium => write!(f, "MEDIUM"),
            Tier::Complex => write!(f, "COMPLEX"),
            Tier::Reasoning => write!(f, "REASONING"),
        }
    }
}

// ---------------------------------------------------------------------------
// Routing profiles
// ---------------------------------------------------------------------------

/// Routing profile that controls which tier→model mapping is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingProfile {
    /// Standard routing with automatic agentic detection.
    #[default]
    Auto,
    /// Ultra cost-optimized: cheapest viable models per tier.
    Eco,
    /// Best quality: premium models, no savings calculation.
    Premium,
    /// Free tier: uses only zero-cost models.
    Free,
}

impl std::str::FromStr for RoutingProfile {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "eco" | "economy" | "cheap" => Ok(Self::Eco),
            "premium" | "quality" | "best" => Ok(Self::Premium),
            "free" => Ok(Self::Free),
            _ => Err(format!(
                "invalid routing profile '{}', expected: auto, eco, premium, free",
                s
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Model catalog
// ---------------------------------------------------------------------------

/// Model capability flags.
#[derive(Debug, Clone, Default)]
pub struct ModelCapabilities {
    pub reasoning: bool,
    pub vision: bool,
    pub agentic: bool,
}

/// A model in the catalog with pricing and capabilities.
#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: String,
    pub name: String,
    pub input_price_per_million: Decimal,
    pub output_price_per_million: Decimal,
    pub context_window: u64,
    pub max_output: u64,
    pub capabilities: ModelCapabilities,
}

/// Tier configuration: primary model + ordered fallback chain.
#[derive(Debug, Clone)]
pub struct TierConfig {
    pub primary: String,
    pub fallbacks: Vec<String>,
}

impl TierConfig {
    fn chain(&self) -> Vec<&str> {
        let mut c = vec![self.primary.as_str()];
        for f in &self.fallbacks {
            c.push(f.as_str());
        }
        c
    }
}

// ---------------------------------------------------------------------------
// Built-in model catalog
// ---------------------------------------------------------------------------

static MODEL_CATALOG: Lazy<Vec<ModelEntry>> = Lazy::new(|| {
    vec![
        // OpenAI — GPT-4o/4.1/o4-mini retired 2026-02-13
        me("openai/gpt-5.3-codex", "GPT-5.3 Codex", dec!(2.5), dec!(12.0), 128_000, 32_000, true, false, true),
        me("openai/gpt-5.2", "GPT-5.2", dec!(1.75), dec!(14.0), 400_000, 128_000, true, true, true),
        me("openai/gpt-5-mini", "GPT-5 Mini", dec!(0.25), dec!(2.0), 200_000, 65_000, false, false, false),
        me("openai/gpt-5-nano", "GPT-5 Nano", dec!(0.05), dec!(0.4), 128_000, 32_000, false, false, false),
        // Anthropic
        me("anthropic/claude-opus-4.6", "Claude Opus 4.6", dec!(5.0), dec!(25.0), 1_000_000, 128_000, true, true, true),
        me("anthropic/claude-sonnet-4", "Claude Sonnet 4", dec!(3.0), dec!(15.0), 200_000, 64_000, true, false, true),
        me("anthropic/claude-haiku-4.5", "Claude Haiku 4.5", dec!(1.0), dec!(5.0), 200_000, 8_000, false, false, true),
        // Google
        me("google/gemini-3-pro-preview", "Gemini 3 Pro", dec!(2.0), dec!(12.0), 1_050_000, 65_000, true, true, false),
        me("google/gemini-2.5-pro", "Gemini 2.5 Pro", dec!(1.25), dec!(10.0), 1_050_000, 65_000, true, true, false),
        me("google/gemini-2.5-flash", "Gemini 2.5 Flash", dec!(0.15), dec!(0.6), 1_000_000, 65_000, false, false, false),
        // DeepSeek
        me("deepseek/deepseek-chat", "DeepSeek V3.2", dec!(0.28), dec!(0.42), 128_000, 8_000, false, false, false),
        me("deepseek/deepseek-reasoner", "DeepSeek Reasoner", dec!(0.28), dec!(0.42), 128_000, 8_000, true, false, false),
        // Moonshot
        me("moonshot/kimi-k2.5", "Kimi K2.5", dec!(0.5), dec!(2.4), 262_000, 8_000, true, true, true),
        // xAI
        me("xai/grok-4-1-fast-reasoning", "Grok 4.1 Fast Reasoning", dec!(0.2), dec!(0.5), 131_000, 16_000, true, false, false),
        me("xai/grok-code-fast-1", "Grok Code Fast", dec!(0.2), dec!(1.5), 131_000, 16_000, false, false, true),
        me("xai/grok-4-0709", "Grok 4", dec!(0.2), dec!(1.5), 131_000, 16_000, true, false, false),
        // NVIDIA (free tier)
        me("nvidia/gpt-oss-120b", "NVIDIA GPT-OSS 120B", dec!(0.0), dec!(0.0), 128_000, 16_000, false, false, false),
        // Ollama / local (zero cost) — models actually installed on Frack + Frick
        me("ollama/qwen3-coder:30b", "Qwen 3 Coder 30B", dec!(0.0), dec!(0.0), 128_000, 32_000, false, false, true),
        me("ollama/deepseek-r1:70b", "DeepSeek R1 70B", dec!(0.0), dec!(0.0), 128_000, 32_000, true, false, false),
        me("ollama/qwen2.5-coder:32b", "Qwen 2.5 Coder 32B", dec!(0.0), dec!(0.0), 128_000, 32_000, false, false, true),
        me("ollama/gemma3:27b", "Gemma 3 27B", dec!(0.0), dec!(0.0), 128_000, 16_000, false, false, false),
    ]
});

fn me(
    id: &str, name: &str,
    input: Decimal, output: Decimal,
    ctx: u64, max_out: u64,
    reasoning: bool, vision: bool, agentic: bool,
) -> ModelEntry {
    ModelEntry {
        id: id.to_string(),
        name: name.to_string(),
        input_price_per_million: input,
        output_price_per_million: output,
        context_window: ctx,
        max_output: max_out,
        capabilities: ModelCapabilities { reasoning, vision, agentic },
    }
}

// ---------------------------------------------------------------------------
// Default tier configurations
// ---------------------------------------------------------------------------

fn default_auto_tiers() -> HashMap<Tier, TierConfig> {
    let mut m = HashMap::new();
    m.insert(Tier::Simple, TierConfig {
        primary: "ollama/qwen3-coder:30b".into(),
        fallbacks: vec!["google/gemini-2.5-flash".into(), "deepseek/deepseek-chat".into(), "nvidia/gpt-oss-120b".into()],
    });
    m.insert(Tier::Medium, TierConfig {
        primary: "ollama/qwen3-coder:30b".into(),
        fallbacks: vec!["xai/grok-code-fast-1".into(), "google/gemini-2.5-flash".into(), "deepseek/deepseek-chat".into()],
    });
    m.insert(Tier::Complex, TierConfig {
        primary: "ollama/deepseek-r1:70b".into(),
        fallbacks: vec!["anthropic/claude-opus-4.6".into(), "google/gemini-3-pro-preview".into(), "openai/gpt-5.3-codex".into()],
    });
    m.insert(Tier::Reasoning, TierConfig {
        primary: "ollama/deepseek-r1:70b".into(),
        fallbacks: vec!["anthropic/claude-opus-4.6".into(), "deepseek/deepseek-reasoner".into(), "xai/grok-4-1-fast-reasoning".into()],
    });
    m
}

fn default_eco_tiers() -> HashMap<Tier, TierConfig> {
    let mut m = HashMap::new();
    m.insert(Tier::Simple, TierConfig {
        primary: "ollama/qwen3-coder:30b".into(),
        fallbacks: vec!["nvidia/gpt-oss-120b".into(), "deepseek/deepseek-chat".into()],
    });
    m.insert(Tier::Medium, TierConfig {
        primary: "ollama/qwen3-coder:30b".into(),
        fallbacks: vec!["deepseek/deepseek-chat".into(), "google/gemini-2.5-flash".into()],
    });
    m.insert(Tier::Complex, TierConfig {
        primary: "ollama/deepseek-r1:70b".into(),
        fallbacks: vec!["deepseek/deepseek-chat".into(), "google/gemini-2.5-flash".into()],
    });
    m.insert(Tier::Reasoning, TierConfig {
        primary: "ollama/deepseek-r1:70b".into(),
        fallbacks: vec!["deepseek/deepseek-reasoner".into(), "xai/grok-4-1-fast-reasoning".into()],
    });
    m
}

fn default_premium_tiers() -> HashMap<Tier, TierConfig> {
    let mut m = HashMap::new();
    m.insert(Tier::Simple, TierConfig {
        primary: "anthropic/claude-haiku-4.5".into(),
        fallbacks: vec!["ollama/qwen3-coder:30b".into(), "google/gemini-2.5-flash".into()],
    });
    m.insert(Tier::Medium, TierConfig {
        primary: "openai/gpt-5.3-codex".into(),
        fallbacks: vec!["anthropic/claude-sonnet-4".into(), "google/gemini-2.5-pro".into()],
    });
    m.insert(Tier::Complex, TierConfig {
        primary: "anthropic/claude-opus-4.6".into(),
        fallbacks: vec!["openai/gpt-5.3-codex".into(), "anthropic/claude-sonnet-4".into(), "google/gemini-3-pro-preview".into()],
    });
    m.insert(Tier::Reasoning, TierConfig {
        primary: "anthropic/claude-opus-4.6".into(),
        fallbacks: vec!["anthropic/claude-sonnet-4".into(), "openai/gpt-5.2".into(), "xai/grok-4-1-fast-reasoning".into()],
    });
    m
}

fn default_agentic_tiers() -> HashMap<Tier, TierConfig> {
    let mut m = HashMap::new();
    m.insert(Tier::Simple, TierConfig {
        primary: "ollama/qwen3-coder:30b".into(),
        fallbacks: vec!["anthropic/claude-haiku-4.5".into(), "openai/gpt-5-nano".into()],
    });
    m.insert(Tier::Medium, TierConfig {
        primary: "ollama/qwen3-coder:30b".into(),
        fallbacks: vec!["xai/grok-code-fast-1".into(), "anthropic/claude-haiku-4.5".into()],
    });
    m.insert(Tier::Complex, TierConfig {
        primary: "anthropic/claude-opus-4.6".into(),
        fallbacks: vec!["openai/gpt-5.3-codex".into(), "anthropic/claude-sonnet-4".into(), "google/gemini-3-pro-preview".into()],
    });
    m.insert(Tier::Reasoning, TierConfig {
        primary: "anthropic/claude-opus-4.6".into(),
        fallbacks: vec!["anthropic/claude-sonnet-4".into(), "xai/grok-4-1-fast-reasoning".into(), "deepseek/deepseek-reasoner".into()],
    });
    m
}

// ---------------------------------------------------------------------------
// 15-dimension weighted classification
// ---------------------------------------------------------------------------

/// Keyword lists for each classification dimension.
static CODE_KEYWORDS: &[&str] = &[
    "function", "class", "import", "def", "select", "async", "await",
    "const", "let", "var", "return", "```",
];
static REASONING_KEYWORDS: &[&str] = &[
    "prove", "theorem", "derive", "step by step", "chain of thought",
    "formally", "mathematical", "proof", "logically",
];
static SIMPLE_KEYWORDS: &[&str] = &[
    "what is", "define", "translate", "hello", "yes or no",
    "capital of", "how old", "who is", "when was",
];
static TECHNICAL_KEYWORDS: &[&str] = &[
    "algorithm", "optimize", "architecture", "distributed", "kubernetes",
    "microservice", "database", "infrastructure",
];
static CREATIVE_KEYWORDS: &[&str] = &[
    "story", "poem", "compose", "brainstorm", "creative", "imagine", "write a",
];
static IMPERATIVE_KEYWORDS: &[&str] = &[
    "build", "create", "implement", "design", "develop", "construct",
    "generate", "deploy", "configure", "set up",
];
static CONSTRAINT_KEYWORDS: &[&str] = &[
    "under", "at most", "at least", "within", "no more than",
    "maximum", "minimum", "limit", "budget",
];
static OUTPUT_FORMAT_KEYWORDS: &[&str] = &[
    "json", "yaml", "xml", "table", "csv", "markdown", "schema", "format as", "structured",
];
static REFERENCE_KEYWORDS: &[&str] = &[
    "above", "below", "previous", "following", "the docs", "the api",
    "the code", "earlier", "attached",
];
static NEGATION_KEYWORDS: &[&str] = &[
    "don't", "do not", "avoid", "never", "without", "except", "exclude", "no longer",
];
static DOMAIN_KEYWORDS: &[&str] = &[
    "quantum", "fpga", "vlsi", "risc-v", "asic", "photonics", "genomics",
    "proteomics", "topological", "homomorphic", "zero-knowledge", "lattice-based",
];
static AGENTIC_KEYWORDS: &[&str] = &[
    "read file", "read the file", "look at", "check the", "open the",
    "edit", "modify", "update the", "change the", "write to", "create file",
    "execute", "deploy", "install", "npm", "pip", "compile",
    "after that", "and also", "once done", "step 1", "step 2",
    "fix", "debug", "until it works", "keep trying", "iterate",
    "make sure", "verify", "confirm",
];

static MULTI_STEP_RE: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        Regex::new(r"(?i)first.*then").unwrap(),
        Regex::new(r"(?i)step \d").unwrap(),
        Regex::new(r"\d\.\s").unwrap(),
    ]
});

/// Count keyword matches in text (case-insensitive).
fn count_matches(text: &str, keywords: &[&str]) -> usize {
    keywords.iter().filter(|kw| text.contains(*kw)).count()
}

/// Score a single dimension given match count and thresholds.
fn dimension_score(count: usize, low: usize, high: usize, low_val: f64, high_val: f64) -> f64 {
    if count >= high {
        high_val
    } else if count >= low {
        low_val
    } else {
        0.0
    }
}

/// Result of the 15-dimension classification.
#[derive(Debug, Clone)]
pub struct ClassificationResult {
    pub tier: Option<Tier>,
    pub confidence: f64,
    pub weighted_score: f64,
    pub agentic_score: f64,
    pub signals: Vec<String>,
}

/// Classify a request using the 15-dimension weighted scoring system.
pub fn classify(prompt: &str, system_prompt: &str) -> ClassificationResult {
    let full = format!("{} {}", system_prompt, prompt).to_lowercase();
    let user = prompt.to_lowercase();

    let mut signals = Vec::new();
    let mut weighted_score = 0.0;

    // 1. Token count (weight 0.08)
    let token_est = (full.len() + 3) / 4;
    let tc_score = if token_est < 50 { -1.0 } else if token_est > 500 { 1.0 } else { 0.0 };
    weighted_score += tc_score * 0.08;
    if tc_score != 0.0 {
        signals.push(format!("tokens:{}", token_est));
    }

    // 2. Code presence (weight 0.15)
    let code_count = count_matches(&full, CODE_KEYWORDS);
    let code_score = dimension_score(code_count, 1, 2, 0.5, 1.0);
    weighted_score += code_score * 0.15;
    if code_count > 0 { signals.push(format!("code:{}", code_count)); }

    // 3. Reasoning markers — user prompt only (weight 0.18)
    let reasoning_count = count_matches(&user, REASONING_KEYWORDS);
    let reasoning_score = dimension_score(reasoning_count, 1, 2, 0.7, 1.0);
    weighted_score += reasoning_score * 0.18;
    if reasoning_count > 0 { signals.push(format!("reasoning:{}", reasoning_count)); }

    // 4. Technical terms (weight 0.10)
    let tech_count = count_matches(&full, TECHNICAL_KEYWORDS);
    let tech_score = dimension_score(tech_count, 2, 4, 0.5, 1.0);
    weighted_score += tech_score * 0.10;
    if tech_count > 0 { signals.push(format!("technical:{}", tech_count)); }

    // 5. Creative markers (weight 0.05)
    let creative_count = count_matches(&full, CREATIVE_KEYWORDS);
    let creative_score = dimension_score(creative_count, 1, 2, 0.5, 0.7);
    weighted_score += creative_score * 0.05;

    // 6. Simple indicators (weight 0.02) — pulls score DOWN
    let simple_count = count_matches(&full, SIMPLE_KEYWORDS);
    let simple_score = if simple_count > 0 { -1.0 } else { 0.0 };
    weighted_score += simple_score * 0.02;

    // 7. Multi-step patterns (weight 0.12)
    let multi_count = MULTI_STEP_RE.iter().filter(|re: &&Regex| re.is_match(&full)).count();
    let multi_score = if multi_count > 0 { 0.5 } else { 0.0 };
    weighted_score += multi_score * 0.12;
    if multi_count > 0 { signals.push(format!("multistep:{}", multi_count)); }

    // 8. Question complexity (weight 0.05)
    let q_count = full.matches('?').count();
    let q_score = if q_count >= 4 { 0.5 } else { 0.0 };
    weighted_score += q_score * 0.05;

    // 9. Imperative verbs (weight 0.03)
    let imp_count = count_matches(&full, IMPERATIVE_KEYWORDS);
    let imp_score = dimension_score(imp_count, 1, 2, 0.3, 0.5);
    weighted_score += imp_score * 0.03;

    // 10. Constraint count (weight 0.04)
    let con_count = count_matches(&full, CONSTRAINT_KEYWORDS);
    let con_score = dimension_score(con_count, 1, 3, 0.3, 0.7);
    weighted_score += con_score * 0.04;

    // 11. Output format (weight 0.03)
    let fmt_count = count_matches(&full, OUTPUT_FORMAT_KEYWORDS);
    let fmt_score = dimension_score(fmt_count, 1, 2, 0.4, 0.7);
    weighted_score += fmt_score * 0.03;

    // 12. Reference complexity (weight 0.02)
    let ref_count = count_matches(&full, REFERENCE_KEYWORDS);
    let ref_score = dimension_score(ref_count, 1, 2, 0.3, 0.5);
    weighted_score += ref_score * 0.02;

    // 13. Negation complexity (weight 0.01)
    let neg_count = count_matches(&full, NEGATION_KEYWORDS);
    let neg_score = dimension_score(neg_count, 2, 3, 0.3, 0.5);
    weighted_score += neg_score * 0.01;

    // 14. Domain specificity (weight 0.02)
    let dom_count = count_matches(&full, DOMAIN_KEYWORDS);
    let dom_score = dimension_score(dom_count, 1, 2, 0.5, 0.8);
    weighted_score += dom_score * 0.02;

    // 15. Agentic task (weight 0.04)
    let ag_count = count_matches(&full, AGENTIC_KEYWORDS);
    let ag_score = if ag_count >= 4 {
        1.0
    } else if ag_count >= 3 {
        0.6
    } else if ag_count >= 1 {
        0.2
    } else {
        0.0
    };
    weighted_score += ag_score * 0.04;
    let agentic_score = (ag_count as f64 / AGENTIC_KEYWORDS.len() as f64).min(1.0);
    if ag_count > 0 { signals.push(format!("agentic:{}", ag_count)); }

    // OVERRIDE: 2+ reasoning keywords in user text → force REASONING
    if reasoning_count >= 2 {
        let distance = (weighted_score.max(0.3) - 0.0).abs();
        let confidence = sigmoid(distance, 12.0).max(0.85);
        return ClassificationResult {
            tier: Some(Tier::Reasoning),
            confidence,
            weighted_score,
            agentic_score,
            signals,
        };
    }

    // Map score to tier via boundaries
    let tier = if weighted_score < 0.0 {
        Tier::Simple
    } else if weighted_score < 0.3 {
        Tier::Medium
    } else if weighted_score < 0.5 {
        Tier::Complex
    } else {
        Tier::Reasoning
    };

    // Compute confidence from distance to nearest boundary
    let boundaries = [0.0, 0.3, 0.5];
    let min_distance = boundaries
        .iter()
        .map(|b| (weighted_score - b).abs())
        .fold(f64::MAX, f64::min);
    let confidence = sigmoid(min_distance, 12.0);

    let tier_opt = if confidence < 0.7 { None } else { Some(tier) };

    ClassificationResult {
        tier: tier_opt,
        confidence,
        weighted_score,
        agentic_score,
        signals,
    }
}

fn sigmoid(x: f64, steepness: f64) -> f64 {
    1.0 / (1.0 + (-steepness * x).exp())
}

// ---------------------------------------------------------------------------
// Routing decision
// ---------------------------------------------------------------------------

/// The result of routing a request.
#[derive(Debug, Clone)]
pub struct RoutingDecision {
    pub model_id: String,
    pub tier: Tier,
    pub confidence: f64,
    pub agentic: bool,
    pub profile: RoutingProfile,
    pub cost_estimate: CostEstimate,
    pub signals: Vec<String>,
    pub fallback_chain: Vec<String>,
}

/// Estimated costs for a routing decision.
#[derive(Debug, Clone)]
pub struct CostEstimate {
    pub input_cost: Decimal,
    pub output_cost: Decimal,
    pub total_cost: Decimal,
    pub savings_pct: f64,
}

// ---------------------------------------------------------------------------
// Rate-limit cooldown tracker
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CooldownEntry {
    until: Instant,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Configuration for the intelligent LLM router.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterConfig {
    /// Routing profile (auto, eco, premium, free).
    pub profile: RoutingProfile,
    /// Agentic score threshold for auto-switching to agentic tiers.
    pub agentic_threshold: f64,
    /// Force agentic mode regardless of classification.
    pub force_agentic: bool,
    /// Default tier for ambiguous classifications (confidence < 0.7).
    pub ambiguous_default: Tier,
    /// Token threshold above which requests are forced to COMPLEX.
    pub large_request_threshold: u64,
    /// Maximum fallback attempts per request.
    pub max_fallback_attempts: usize,
    /// Free-tier model override.
    pub free_model: String,
    /// Enable session pinning (reuse model within a session).
    pub session_pinning: bool,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            profile: RoutingProfile::Auto,
            agentic_threshold: 0.5,
            force_agentic: false,
            ambiguous_default: Tier::Medium,
            large_request_threshold: 100_000,
            max_fallback_attempts: 5,
            free_model: "nvidia/gpt-oss-120b".into(),
            session_pinning: true,
        }
    }
}

/// Intelligent LLM request router.
///
/// Classifies requests by complexity and routes to the optimal model
/// based on the active routing profile and available providers.
pub struct Router {
    config: RouterConfig,
    auto_tiers: HashMap<Tier, TierConfig>,
    eco_tiers: HashMap<Tier, TierConfig>,
    premium_tiers: HashMap<Tier, TierConfig>,
    agentic_tiers: HashMap<Tier, TierConfig>,
    catalog: HashMap<String, ModelEntry>,
    session_pins: Mutex<HashMap<String, String>>,
    cooldowns: Mutex<HashMap<String, CooldownEntry>>,
    requests_routed: AtomicUsize,
}

impl Router {
    /// Create a new router with default tier configurations.
    pub fn new(config: RouterConfig) -> Self {
        let catalog: HashMap<String, ModelEntry> = MODEL_CATALOG
            .iter()
            .map(|m| (m.id.clone(), m.clone()))
            .collect();

        Self {
            config,
            auto_tiers: default_auto_tiers(),
            eco_tiers: default_eco_tiers(),
            premium_tiers: default_premium_tiers(),
            agentic_tiers: default_agentic_tiers(),
            catalog,
            session_pins: Mutex::new(HashMap::new()),
            cooldowns: Mutex::new(HashMap::new()),
            requests_routed: AtomicUsize::new(0),
        }
    }

    /// Route a request to the optimal model.
    ///
    /// Returns the model ID, tier, and fallback chain.
    pub fn route(
        &self,
        prompt: &str,
        system_prompt: &str,
        estimated_input_tokens: Option<u64>,
        session_id: Option<&str>,
    ) -> RoutingDecision {
        self.requests_routed.fetch_add(1, Ordering::Relaxed);

        // Check session pin first
        if self.config.session_pinning {
            if let Some(sid) = session_id {
                if let Ok(pins) = self.session_pins.lock() {
                    if let Some(pinned) = pins.get(sid) {
                        let model = self.catalog.get(pinned);
                        let cost = model
                            .map(|m| self.estimate_cost(m, estimated_input_tokens.unwrap_or(1000)))
                            .unwrap_or_else(|| CostEstimate {
                                input_cost: Decimal::ZERO,
                                output_cost: Decimal::ZERO,
                                total_cost: Decimal::ZERO,
                                savings_pct: 0.0,
                            });
                        return RoutingDecision {
                            model_id: pinned.clone(),
                            tier: Tier::Medium,
                            confidence: 1.0,
                            agentic: false,
                            profile: self.config.profile,
                            cost_estimate: cost,
                            signals: vec!["session-pinned".to_string()],
                            fallback_chain: vec![],
                        };
                    }
                }
            }
        }

        // Free profile bypasses classification
        if self.config.profile == RoutingProfile::Free {
            return RoutingDecision {
                model_id: self.config.free_model.clone(),
                tier: Tier::Simple,
                confidence: 1.0,
                agentic: false,
                profile: RoutingProfile::Free,
                cost_estimate: CostEstimate {
                    input_cost: Decimal::ZERO,
                    output_cost: Decimal::ZERO,
                    total_cost: Decimal::ZERO,
                    savings_pct: 1.0,
                },
                signals: vec!["free-tier".to_string()],
                fallback_chain: vec![],
            };
        }

        // Classify
        let classification = classify(prompt, system_prompt);
        let input_tokens = estimated_input_tokens
            .unwrap_or_else(|| ((prompt.len() + system_prompt.len() + 3) / 4) as u64);

        // Override: large requests → COMPLEX
        let mut tier = classification.tier.unwrap_or(self.config.ambiguous_default);
        let mut signals = classification.signals;

        if input_tokens > self.config.large_request_threshold {
            tier = Tier::Complex;
            signals.push(format!("large-request:{}", input_tokens));
        }

        // Override: structured output in system prompt → minimum MEDIUM
        if tier == Tier::Simple {
            let sys_lower = system_prompt.to_lowercase();
            if sys_lower.contains("json") || sys_lower.contains("structured") || sys_lower.contains("schema") {
                tier = Tier::Medium;
                signals.push("structured-output-upgrade".to_string());
            }
        }

        // Select tier map based on profile + agentic detection
        let is_agentic = self.config.force_agentic
            || (self.config.profile == RoutingProfile::Auto
                && classification.agentic_score >= self.config.agentic_threshold);

        let tier_map = match self.config.profile {
            RoutingProfile::Auto if is_agentic => &self.agentic_tiers,
            RoutingProfile::Auto => &self.auto_tiers,
            RoutingProfile::Eco => &self.eco_tiers,
            RoutingProfile::Premium => &self.premium_tiers,
            RoutingProfile::Free => unreachable!(),
        };

        if is_agentic {
            signals.push(format!("agentic-mode:{:.2}", classification.agentic_score));
        }

        // Get tier config and build fallback chain
        let tier_config = tier_map.get(&tier).expect("all tiers must be configured");
        let chain: Vec<String> = tier_config.chain().iter().map(|s| s.to_string()).collect();

        // Filter chain by context window and cooldowns
        let filtered: Vec<String> = chain
            .iter()
            .filter(|model_id| {
                // Context window check
                if let Some(entry) = self.catalog.get(model_id.as_str()) {
                    let required = (input_tokens as f64 * 1.1) as u64;
                    if entry.context_window < required {
                        return false;
                    }
                }
                // Cooldown check
                if let Ok(cds) = self.cooldowns.lock() {
                    if let Some(cd) = cds.get(model_id.as_str()) {
                        if cd.until > Instant::now() {
                            return false;
                        }
                    }
                }
                true
            })
            .take(self.config.max_fallback_attempts)
            .cloned()
            .collect();

        let selected = filtered.first().unwrap_or(&tier_config.primary).clone();
        let cost = self
            .catalog
            .get(&selected)
            .map(|m| self.estimate_cost(m, input_tokens))
            .unwrap_or_else(|| CostEstimate {
                input_cost: Decimal::ZERO,
                output_cost: Decimal::ZERO,
                total_cost: Decimal::ZERO,
                savings_pct: 0.0,
            });

        // Pin session
        if self.config.session_pinning {
            if let Some(sid) = session_id {
                if let Ok(mut pins) = self.session_pins.lock() {
                    pins.insert(sid.to_string(), selected.clone());
                }
            }
        }

        tracing::debug!(
            tier = %tier,
            model = %selected,
            confidence = classification.confidence,
            agentic = is_agentic,
            profile = ?self.config.profile,
            "Router classified request"
        );

        RoutingDecision {
            model_id: selected,
            tier,
            confidence: classification.confidence,
            agentic: is_agentic,
            profile: self.config.profile,
            cost_estimate: cost,
            signals,
            fallback_chain: filtered,
        }
    }

    /// Mark a model as rate-limited (60s cooldown).
    pub fn mark_rate_limited(&self, model_id: &str) {
        if let Ok(mut cds) = self.cooldowns.lock() {
            cds.insert(model_id.to_string(), CooldownEntry {
                until: Instant::now() + Duration::from_secs(60),
            });
        }
    }

    /// Clear session pin for a session.
    pub fn clear_session_pin(&self, session_id: &str) {
        if let Ok(mut pins) = self.session_pins.lock() {
            pins.remove(session_id);
        }
    }

    /// Get the number of requests routed.
    pub fn requests_routed(&self) -> usize {
        self.requests_routed.load(Ordering::Relaxed)
    }

    /// Look up a model in the catalog.
    pub fn model(&self, id: &str) -> Option<&ModelEntry> {
        self.catalog.get(id)
    }

    fn estimate_cost(&self, model: &ModelEntry, input_tokens: u64) -> CostEstimate {
        let max_output = model.max_output.min(4096); // conservative estimate
        let input_cost = Decimal::from(input_tokens) * model.input_price_per_million / dec!(1_000_000);
        let output_cost = Decimal::from(max_output) * model.output_price_per_million / dec!(1_000_000);
        let total = input_cost + output_cost;

        // Baseline: Claude Opus 4.5 pricing ($5/$25 per M)
        let baseline_input = Decimal::from(input_tokens) * dec!(5.0) / dec!(1_000_000);
        let baseline_output = Decimal::from(max_output) * dec!(25.0) / dec!(1_000_000);
        let baseline = baseline_input + baseline_output;

        let savings_pct = if baseline > Decimal::ZERO {
            let savings = baseline - total;
            let ratio = savings / baseline;
            ratio
                .try_into()
                .unwrap_or(0.0f64)
                .max(0.0)
        } else {
            0.0
        };

        CostEstimate {
            input_cost,
            output_cost,
            total_cost: total,
            savings_pct,
        }
    }
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field("profile", &self.config.profile)
            .field("requests_routed", &self.requests_routed.load(Ordering::Relaxed))
            .field("catalog_size", &self.catalog.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_greeting_routes_to_simple() {
        let result = classify("hello", "");
        assert!(
            result.weighted_score < 0.0,
            "greeting should score below 0.0, got {}",
            result.weighted_score
        );
    }

    #[test]
    fn code_request_scores_higher() {
        let result = classify(
            "implement a function that sorts an array using async await",
            "",
        );
        assert!(
            result.weighted_score > 0.0,
            "code request should score above 0.0, got {}",
            result.weighted_score
        );
    }

    #[test]
    fn reasoning_override_forces_reasoning_tier() {
        let result = classify(
            "prove the theorem step by step and derive the proof logically",
            "",
        );
        assert_eq!(result.tier, Some(Tier::Reasoning));
        assert!(result.confidence >= 0.85);
    }

    #[test]
    fn agentic_detection() {
        let result = classify(
            "read the file, fix the bug, deploy it, make sure it works, verify",
            "",
        );
        assert!(
            result.agentic_score > 0.0,
            "agentic score should be positive"
        );
    }

    #[test]
    fn router_routes_simple_request() {
        let router = Router::new(RouterConfig::default());
        let decision = router.route("hello", "", None, None);
        assert_eq!(decision.tier, Tier::Simple);
    }

    #[test]
    fn router_routes_complex_request() {
        let router = Router::new(RouterConfig::default());
        let decision = router.route(
            "implement a distributed kubernetes microservice with database optimization and infrastructure architecture",
            "",
            None,
            None,
        );
        // Technical + code + imperative keywords push this above Simple
        assert!(
            matches!(decision.tier, Tier::Medium | Tier::Complex | Tier::Reasoning),
            "complex request should route above Simple, got {:?}",
            decision.tier
        );
        // Verify the weighted score reflects real complexity
        let class = classify(
            "implement a distributed kubernetes microservice with database optimization and infrastructure architecture",
            "",
        );
        assert!(
            class.weighted_score > 0.0,
            "complex request should score above 0.0, got {}",
            class.weighted_score
        );
    }

    #[test]
    fn free_profile_always_returns_free_model() {
        let router = Router::new(RouterConfig {
            profile: RoutingProfile::Free,
            ..Default::default()
        });
        let decision = router.route("build a complex system", "", None, None);
        assert_eq!(decision.model_id, "nvidia/gpt-oss-120b");
    }

    #[test]
    fn session_pinning_reuses_model() {
        let router = Router::new(RouterConfig::default());
        let d1 = router.route("hello", "", None, Some("session-1"));
        let d2 = router.route(
            "now implement a complex distributed system",
            "",
            None,
            Some("session-1"),
        );
        assert_eq!(d1.model_id, d2.model_id, "session pinning should reuse model");
    }

    #[test]
    fn large_request_forces_complex() {
        let router = Router::new(RouterConfig::default());
        let decision = router.route("hello", "", Some(150_000), None);
        assert_eq!(decision.tier, Tier::Complex);
    }

    #[test]
    fn rate_limit_cooldown() {
        let router = Router::new(RouterConfig::default());
        let d1 = router.route("hello", "", None, None);
        router.mark_rate_limited(&d1.model_id);
        router.clear_session_pin(""); // clear any pin
        let d2 = router.route("hello", "", None, None);
        // The model may differ if the primary is on cooldown
        // (depends on fallback chain), but routing should succeed
        assert!(!d2.model_id.is_empty());
    }

    #[test]
    fn eco_profile_selects_cheaper_models() {
        let router = Router::new(RouterConfig {
            profile: RoutingProfile::Eco,
            ..Default::default()
        });
        let decision = router.route(
            "explain how kubernetes works",
            "",
            None,
            None,
        );
        // Eco should prefer cheaper models
        let model = router.model(&decision.model_id);
        if let Some(m) = model {
            assert!(
                m.input_price_per_million <= dec!(1.0),
                "eco profile should prefer models under $1/M input"
            );
        }
    }

    #[test]
    fn cost_estimate_calculation() {
        let router = Router::new(RouterConfig::default());
        let decision = router.route("hello", "", Some(1000), None);
        // Cost should be non-negative
        assert!(decision.cost_estimate.total_cost >= Decimal::ZERO);
        assert!(decision.cost_estimate.savings_pct >= 0.0);
    }

    #[test]
    fn local_first_default_routing() {
        let router = Router::new(RouterConfig::default());
        // Simple and medium requests should prefer local Ollama models
        let simple = router.route("hello", "", None, None);
        assert!(
            simple.model_id.starts_with("ollama/"),
            "simple requests should default to local Ollama, got {}",
            simple.model_id
        );
        router.clear_session_pin("");
        let medium = router.route("explain how kubernetes works", "", None, None);
        assert!(
            medium.model_id.starts_with("ollama/"),
            "medium requests should default to local Ollama, got {}",
            medium.model_id
        );
    }

    #[test]
    fn opus_46_is_complex_fallback() {
        let router = Router::new(RouterConfig::default());
        let decision = router.route(
            "implement a distributed kubernetes microservice with database optimization and infrastructure architecture",
            "",
            None,
            None,
        );
        // For complex/reasoning tiers, Opus 4.6 should be in the fallback chain
        if matches!(decision.tier, Tier::Complex | Tier::Reasoning) {
            assert!(
                decision.fallback_chain.iter().any(|m| m.contains("claude-opus-4.6")),
                "Opus 4.6 should be in complex/reasoning fallback chain: {:?}",
                decision.fallback_chain
            );
        }
    }

    #[test]
    fn no_retired_models_in_catalog() {
        let router = Router::new(RouterConfig::default());
        // GPT-4o, GPT-4.1, o4-mini were retired 2026-02-13
        assert!(router.model("openai/gpt-4o").is_none(), "GPT-4o should be removed (retired)");
        assert!(router.model("openai/gpt-4o-mini").is_none(), "GPT-4o Mini should be removed (retired)");
        assert!(router.model("openai/o4-mini").is_none(), "o4-mini should be removed (retired)");
        assert!(router.model("openai/o3").is_none(), "o3 should be removed (retired)");
        // GPT-5.3-Codex should be present
        assert!(router.model("openai/gpt-5.3-codex").is_some(), "GPT-5.3 Codex should be in catalog");
    }
}
