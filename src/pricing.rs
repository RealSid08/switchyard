//! Token prices and per-event cost estimates.
//!
//! Prices are integer micro-USD per million tokens (`$1.25 / MTok` is `1_250_000`), and costs are
//! integer micro-USD computed with checked `u128` arithmetic, rounded half up once per event.
//! Every estimate records the rate card it used (`version`), so recorded events keep the price
//! they were recorded with when the card or a user override changes later.
//!
//! The official card below was read from the providers' published pricing pages on 2026-10-03:
//! - Anthropic: <https://platform.claude.com/docs/en/about-claude/pricing>
//! - OpenAI: <https://developers.openai.com/api/docs/pricing>
//! - Google Gemini API: <https://ai.google.dev/gemini-api/docs/pricing> (page dated 2026-10-01)
//!
//! Scope of the estimate (standard, synchronous, global endpoints). Not modelled, so not priced
//! differently: Anthropic fast mode and US data residency (1.1x), batch discounts, priority or
//! flex tiers, server-side tool fees (web search), Gemini context-cache storage per hour.
//! Unknown models, unknown token dimensions and modality-specific Gemini audio rates make an
//! event unpriced (`cost = None`) rather than guessed.
use serde_json::{Value, json};

use crate::usage::Tokens;

/// Version label of the built-in official rate card.
pub const OFFICIAL_VERSION: &str = "official-2026-10-03";
/// Date the official card was read from the provider pages.
pub const OFFICIAL_AS_OF: &str = "2026-10-03";
/// Upper bound for user-supplied rates: 10,000 USD per million tokens.
pub const MAX_RATE_MICROS: u64 = 10_000 * 1_000_000;

pub const SOURCES: [(&str, &str); 4] = [
    (
        "anthropic",
        "https://platform.claude.com/docs/en/about-claude/pricing",
    ),
    ("openai", "https://developers.openai.com/api/docs/pricing"),
    ("gemini", "https://ai.google.dev/gemini-api/docs/pricing"),
    ("opencode_go", "https://opencode.ai/docs/go/"),
];

/// Prices for one tier, micro-USD per million tokens. `None` means the provider publishes no
/// price for that dimension (tokens in it make the event unpriced).
#[derive(Clone, Copy, Debug, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Rate {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    /// Cache writes without a TTL breakdown (OpenAI's single cache-write rate; Anthropic's
    /// default 5-minute TTL).
    pub cache_write: Option<u64>,
    pub cache_write_5m: Option<u64>,
    pub cache_write_1h: Option<u64>,
}

/// One rate card entry.
#[derive(Clone, Debug)]
pub struct Card {
    pub model: String,
    pub provider: String,
    pub base: Rate,
    /// `(threshold, rate)`: requests whose total input exceeds `threshold` tokens are priced
    /// entirely at `rate` (the providers' "prompts > N tokens" tiers).
    pub long: Option<(u64, Rate)>,
    /// Inclusive UTC day range (`YYYY-MM-DD`) in which this entry applies.
    pub from_day: Option<String>,
    pub until_day: Option<String>,
    /// Models whose audio input has a different published price.
    pub audio_distinct: bool,
    pub origin: &'static str,
    pub version: String,
    pub note: Option<&'static str>,
}

const fn usd(micro: u64) -> Option<u64> {
    Some(micro)
}

struct Official {
    models: &'static [&'static str],
    provider: &'static str,
    base: Rate,
    long: Option<(u64, Rate)>,
    from_day: Option<&'static str>,
    until_day: Option<&'static str>,
    audio_distinct: bool,
    note: Option<&'static str>,
}

/// Anthropic rate: input, 5m write, 1h write, cache read, output (micro-USD / MTok).
const fn anthropic(input: u64, w5: u64, w1h: u64, read: u64, output: u64) -> Rate {
    Rate {
        input: usd(input),
        output: usd(output),
        cache_read: usd(read),
        cache_write: usd(w5),
        cache_write_5m: usd(w5),
        cache_write_1h: usd(w1h),
    }
}
/// OpenAI rate: input, cached input, cache write (0 = not published), output.
const fn openai(input: u64, cached: u64, write: u64, output: u64) -> Rate {
    Rate {
        input: usd(input),
        output: usd(output),
        cache_read: if cached == 0 { None } else { usd(cached) },
        cache_write: if write == 0 { None } else { usd(write) },
        cache_write_5m: None,
        cache_write_1h: None,
    }
}
/// Gemini rate: input, output (including thinking), context-cache read (0 = not available).
const fn gemini(input: u64, output: u64, cached: u64) -> Rate {
    Rate {
        input: usd(input),
        output: usd(output),
        cache_read: if cached == 0 { None } else { usd(cached) },
        cache_write: None,
        cache_write_5m: None,
        cache_write_1h: None,
    }
}

const M: u64 = 1_000_000;
const OPENAI_LONG: u64 = 272_000;
const GEMINI_LONG: u64 = 200_000;

const fn plain(models: &'static [&'static str], provider: &'static str, base: Rate) -> Official {
    Official {
        models,
        provider,
        base,
        long: None,
        from_day: None,
        until_day: None,
        audio_distinct: false,
        note: None,
    }
}

static OFFICIAL: &[Official] = &[
    // Go rates are namespaced because the same model can have different provider prices.
    plain(
        &["opencode_go/glm-5.3-flash"],
        "opencode_go",
        openai(150000, 30000, 0, 500000),
    ),
    plain(
        &["opencode_go/glm-5.3"],
        "opencode_go",
        openai(1400000, 260000, 0, 4400000),
    ),
    plain(
        &["opencode_go/glm-5.2"],
        "opencode_go",
        openai(1400000, 260000, 0, 4400000),
    ),
    plain(
        &["opencode_go/kimi-k3"],
        "opencode_go",
        openai(3000000, 300000, 0, 15000000),
    ),
    plain(
        &["opencode_go/kimi-k2.7-code"],
        "opencode_go",
        openai(950000, 190000, 0, 4000000),
    ),
    plain(
        &["opencode_go/kimi-k2.6"],
        "opencode_go",
        openai(950000, 160000, 0, 4000000),
    ),
    plain(
        &["opencode_go/longcat-2.0"],
        "opencode_go",
        openai(300000, 6000, 0, 1200000),
    ),
    plain(
        &["opencode_go/mimo-v2.6-flash"],
        "opencode_go",
        openai(140000, 2800, 0, 280000),
    ),
    plain(
        &["opencode_go/mimo-v2.6-pro"],
        "opencode_go",
        openai(435000, 3625, 0, 870000),
    ),
    plain(
        &["opencode_go/mimo-v2.5"],
        "opencode_go",
        openai(140000, 2800, 0, 280000),
    ),
    plain(
        &["opencode_go/mimo-v2.5-pro"],
        "opencode_go",
        openai(435000, 3625, 0, 870000),
    ),
    plain(
        &["opencode_go/minimax-m3"],
        "opencode_go",
        openai(300000, 60000, 0, 1200000),
    ),
    plain(
        &["opencode_go/minimax-m2.7"],
        "opencode_go",
        openai(300000, 60000, 375000, 1200000),
    ),
    plain(
        &["opencode_go/muse-spark-1.3-contributor"],
        "opencode_go",
        openai(100000, 20000, 0, 200000),
    ),
    plain(
        &["opencode_go/muse-spark-1.2-contributor"],
        "opencode_go",
        openai(100000, 20000, 0, 200000),
    ),
    plain(
        &["opencode_go/qwen3.8-max"],
        "opencode_go",
        openai(2000000, 250000, 2500000, 6000000),
    ),
    plain(
        &["opencode_go/qwen3.8-flash"],
        "opencode_go",
        openai(150000, 16000, 200000, 470000),
    ),
    Official {
        note: Some(
            "DeepSeek weekday peaks 01:00-04:00 and 06:00-10:00 UTC cost twice these off-peak rates.",
        ),
        ..plain(
            &["opencode_go/deepseek-v4.1-flash"],
            "opencode_go",
            openai(150000, 3000, 0, 600000),
        )
    },
    Official {
        note: Some(
            "DeepSeek weekday peaks 01:00-04:00 and 06:00-10:00 UTC cost twice these off-peak rates.",
        ),
        ..plain(
            &["opencode_go/deepseek-v4-flash"],
            "opencode_go",
            openai(150000, 3000, 0, 600000),
        )
    },
    Official {
        note: Some(
            "DeepSeek weekday peaks 01:00-04:00 and 06:00-10:00 UTC cost twice these off-peak rates.",
        ),
        ..plain(
            &["opencode_go/deepseek-v4-flash-vision-exp"],
            "opencode_go",
            openai(150000, 3000, 0, 600000),
        )
    },
    Official {
        note: Some(
            "DeepSeek weekday peaks 01:00-04:00 and 06:00-10:00 UTC cost twice these off-peak rates.",
        ),
        ..plain(
            &["opencode_go/deepseek-v4-pro"],
            "opencode_go",
            openai(660000, 22000, 0, 1980000),
        )
    },
    plain(
        &["opencode_go/hy4-preview"],
        "opencode_go",
        openai(834000, 42000, 0, 2501000),
    ),
    plain(
        &["opencode_go/hy3"],
        "opencode_go",
        openai(140000, 35000, 0, 580000),
    ),
    Official {
        long: Some((256000, openai(1200000, 120000, 1500000, 4800000))),
        ..plain(
            &["opencode_go/qwen3.7-plus"],
            "opencode_go",
            openai(400000, 40000, 500000, 1600000),
        )
    },
    Official {
        long: Some((200000, openai(4000000, 1000000, 0, 12000000))),
        ..plain(
            &["opencode_go/grok-4.7"],
            "opencode_go",
            openai(2000000, 500000, 0, 6000000),
        )
    },
    Official {
        long: Some((200000, openai(4000000, 1000000, 0, 12000000))),
        ..plain(
            &["opencode_go/grok-4.6"],
            "opencode_go",
            openai(2000000, 500000, 0, 6000000),
        )
    },
    Official {
        long: Some((272000, openai(200000, 20000, 250000, 750000))),
        ..plain(
            &["opencode_go/gpt-6-luna"],
            "opencode_go",
            openai(100000, 10000, 125000, 500000),
        )
    },
    Official {
        long: Some((272000, openai(400000, 40000, 500000, 1800000))),
        ..plain(
            &["opencode_go/gpt-5.6-luna"],
            "opencode_go",
            openai(200000, 20000, 250000, 1200000),
        )
    },
    // Anthropic. Claude 4.6 and later include the 1M context at standard prices (no long tier).
    plain(
        &["claude-fable-5-1", "claude-mythos-5-1"],
        "anthropic",
        anthropic(10 * M, 12_500_000, 20 * M, 250_000, 50 * M),
    ),
    plain(
        &["claude-fable-5", "claude-mythos-5"],
        "anthropic",
        anthropic(10 * M, 12_500_000, 20 * M, M, 50 * M),
    ),
    plain(
        &["claude-opus-5-5"],
        "anthropic",
        anthropic(4 * M, 5 * M, 8 * M, 200_000, 20 * M),
    ),
    plain(
        &[
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-opus-4-5",
        ],
        "anthropic",
        anthropic(5 * M, 6_250_000, 10 * M, 500_000, 25 * M),
    ),
    plain(
        &["claude-opus-4-1", "claude-opus-4"],
        "anthropic",
        anthropic(15 * M, 18_750_000, 30 * M, 1_500_000, 75 * M),
    ),
    plain(
        &["claude-sonnet-5-5", "claude-sonnet-5"],
        "anthropic",
        anthropic(2 * M, 2_500_000, 4 * M, 200_000, 10 * M),
    ),
    plain(
        &["claude-sonnet-4-6", "claude-sonnet-4-5", "claude-sonnet-4"],
        "anthropic",
        anthropic(3 * M, 3_750_000, 6 * M, 300_000, 15 * M),
    ),
    plain(
        &["claude-haiku-4-5"],
        "anthropic",
        anthropic(M, 1_250_000, 2 * M, 100_000, 5 * M),
    ),
    plain(
        &["claude-3-5-haiku"],
        "anthropic",
        anthropic(800_000, M, 1_600_000, 80_000, 4 * M),
    ),
    // OpenAI standard tier. Long context: requests with > 272K input tokens.
    Official {
        long: Some((OPENAI_LONG, openai(20 * M, 2 * M, 25 * M, 75 * M))),
        ..plain(
            &["gpt-6-astra"],
            "openai",
            openai(10 * M, M, 12_500_000, 50 * M),
        )
    },
    Official {
        long: Some((OPENAI_LONG, openai(4 * M, 200_000, 5 * M, 15 * M))),
        ..plain(
            &["gpt-6.1-sol"],
            "openai",
            openai(2 * M, 100_000, 2_500_000, 10 * M),
        )
    },
    Official {
        long: Some((OPENAI_LONG, openai(200_000, 20_000, 250_000, 750_000))),
        ..plain(
            &["gpt-6-luna"],
            "openai",
            openai(100_000, 10_000, 125_000, 500_000),
        )
    },
    Official {
        long: Some((OPENAI_LONG, openai(4 * M, 400_000, 5 * M, 15 * M))),
        ..plain(
            &["gpt-6-sol"],
            "openai",
            openai(2 * M, 200_000, 2_500_000, 10 * M),
        )
    },
    Official {
        long: Some((OPENAI_LONG, openai(8 * M, 800_000, 10 * M, 30 * M))),
        ..plain(
            &["gpt-5.6-sol"],
            "openai",
            openai(4 * M, 400_000, 5 * M, 20 * M),
        )
    },
    Official {
        long: Some((OPENAI_LONG, openai(4 * M, 400_000, 5 * M, 18 * M))),
        ..plain(
            &["gpt-5.6-terra"],
            "openai",
            openai(2 * M, 200_000, 2_500_000, 12 * M),
        )
    },
    Official {
        long: Some((OPENAI_LONG, openai(400_000, 40_000, 500_000, 1_800_000))),
        ..plain(
            &["gpt-5.6-luna"],
            "openai",
            openai(200_000, 20_000, 250_000, 1_200_000),
        )
    },
    Official {
        long: Some((OPENAI_LONG, openai(10 * M, M, 0, 45 * M))),
        ..plain(&["gpt-5.5"], "openai", openai(5 * M, 500_000, 0, 30 * M))
    },
    Official {
        long: Some((OPENAI_LONG, openai(60 * M, 0, 0, 270 * M))),
        ..plain(
            &["gpt-5.5-pro", "gpt-5.4-pro"],
            "openai",
            openai(30 * M, 0, 0, 180 * M),
        )
    },
    Official {
        long: Some((OPENAI_LONG, openai(5 * M, 500_000, 0, 22_500_000))),
        ..plain(
            &["gpt-5.4"],
            "openai",
            openai(2_500_000, 250_000, 0, 15 * M),
        )
    },
    plain(
        &["gpt-5.4-mini"],
        "openai",
        openai(750_000, 75_000, 0, 4_500_000),
    ),
    plain(
        &["gpt-5.4-nano"],
        "openai",
        openai(200_000, 20_000, 0, 1_250_000),
    ),
    plain(
        &["gpt-5.2", "gpt-5.3-codex"],
        "openai",
        openai(1_750_000, 175_000, 0, 14 * M),
    ),
    plain(&["gpt-5.2-pro"], "openai", openai(21 * M, 0, 0, 168 * M)),
    plain(
        &["gpt-5.1", "gpt-5"],
        "openai",
        openai(1_250_000, 125_000, 0, 10 * M),
    ),
    plain(&["gpt-5-mini"], "openai", openai(250_000, 25_000, 0, 2 * M)),
    plain(&["gpt-5-nano"], "openai", openai(50_000, 5_000, 0, 400_000)),
    plain(&["gpt-5-pro"], "openai", openai(15 * M, 0, 0, 120 * M)),
    // Google Gemini API, paid tier, standard. Output prices include thinking tokens.
    Official {
        until_day: Some("2026-12-31"),
        note: Some("Published price through 2026-12-31"),
        ..plain(
            &["gemini-3.8-flash", "gemini-3.7-flash", "gemini-3.6-flash"],
            "gemini",
            gemini(750_000, 3_750_000, 75_000),
        )
    },
    Official {
        from_day: Some("2027-01-01"),
        note: Some("Published price starting 2027-01-01"),
        ..plain(
            &["gemini-3.8-flash", "gemini-3.7-flash", "gemini-3.6-flash"],
            "gemini",
            gemini(1_500_000, 7_500_000, 150_000),
        )
    },
    plain(
        &["gemini-3.5-flash"],
        "gemini",
        gemini(1_500_000, 9 * M, 150_000),
    ),
    plain(
        &["gemini-3.5-flash-lite"],
        "gemini",
        gemini(300_000, 2_500_000, 30_000),
    ),
    Official {
        audio_distinct: true,
        ..plain(
            &["gemini-3.1-flash-lite"],
            "gemini",
            gemini(250_000, 1_500_000, 25_000),
        )
    },
    Official {
        long: Some((GEMINI_LONG, gemini(4 * M, 18 * M, 400_000))),
        ..plain(
            &["gemini-3.1-pro-preview"],
            "gemini",
            gemini(2 * M, 12 * M, 200_000),
        )
    },
    Official {
        long: Some((GEMINI_LONG, gemini(2_500_000, 15 * M, 0))),
        ..plain(&["gemini-2.5-pro"], "gemini", gemini(1_250_000, 10 * M, 0))
    },
    Official {
        audio_distinct: true,
        ..plain(
            &["gemini-2.5-flash"],
            "gemini",
            gemini(300_000, 2_500_000, 30_000),
        )
    },
    Official {
        audio_distinct: true,
        ..plain(
            &["gemini-2.5-flash-lite"],
            "gemini",
            gemini(100_000, 400_000, 10_000),
        )
    },
];

/// The built-in official card, one entry per model and effective range.
pub fn official_cards() -> Vec<Card> {
    OFFICIAL
        .iter()
        .flat_map(|o| {
            o.models.iter().map(|m| Card {
                model: (*m).to_string(),
                provider: o.provider.to_string(),
                base: o.base,
                long: o.long,
                from_day: o.from_day.map(String::from),
                until_day: o.until_day.map(String::from),
                audio_distinct: o.audio_distinct,
                origin: "official",
                version: OFFICIAL_VERSION.to_string(),
                note: o.note,
            })
        })
        .collect()
}

/// A user override, stored in the key-value store (kind [`OVERRIDE_KIND`], id = model).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Override {
    pub model: String,
    pub rate: Rate,
    pub updated_at: String,
}
pub const OVERRIDE_KIND: &str = "usage_price_override";
pub const MAX_OVERRIDES: usize = 200;

impl Override {
    fn card(&self) -> Card {
        Card {
            model: self.model.clone(),
            provider: String::new(),
            base: self.rate,
            long: None,
            from_day: None,
            until_day: None,
            audio_distinct: false,
            origin: "override",
            version: format!("override-{}", self.updated_at),
            note: None,
        }
    }
}

/// Lookup key for a model name: lower case, without a provider prefix (`models/`,
/// `anthropic/`, `openai/`, `google/`) or a trailing `-YYYYMMDD` / `-latest` snapshot suffix.
pub fn normalize_model(model: &str) -> String {
    let mut m = model.trim().to_ascii_lowercase();
    for prefix in ["models/", "anthropic/", "openai/", "google/", "gemini/"] {
        if let Some(rest) = m.strip_prefix(prefix) {
            m = rest.to_string();
        }
    }
    if let Some(rest) = m.strip_suffix("-latest") {
        m = rest.to_string();
    }
    if let Some((head, tail)) = m.rsplit_once('-')
        && tail.len() == 8
        && tail.bytes().all(|b| b.is_ascii_digit())
    {
        m = head.to_string();
    }
    m
}

/// The card for `model` on UTC day `day` (`YYYY-MM-DD`): an override for the exact or normalized
/// name wins over the official card.
pub fn find(model: &str, day: &str, overrides: &[Override]) -> Option<Card> {
    let key = normalize_model(model);
    if let Some(o) = overrides
        .iter()
        .find(|o| o.model == model)
        .or_else(|| overrides.iter().find(|o| normalize_model(&o.model) == key))
    {
        return Some(o.card());
    }
    official_cards().into_iter().find(|c| {
        c.model == key
            && c.from_day.as_deref().is_none_or(|f| day >= f)
            && c.until_day.as_deref().is_none_or(|u| day <= u)
    })
}

/// Result of pricing one event.
#[derive(Clone, Debug, PartialEq)]
pub struct Priced {
    /// Estimated cost in micro-USD; `None` when it cannot be estimated honestly.
    pub cost_micros: Option<u64>,
    /// Rate card version used (`official-…` or `override-…`), when a card was found.
    pub version: Option<String>,
    /// Why the event is unpriced, when it is.
    pub reason: Option<&'static str>,
}
impl Priced {
    fn unpriced(reason: &'static str, version: Option<String>) -> Self {
        Self {
            cost_micros: None,
            version,
            reason: Some(reason),
        }
    }
}

/// Estimates the cost of `tokens` for `model` on `day`.
pub fn price(model: &str, day: &str, tokens: &Tokens, overrides: &[Override]) -> Priced {
    let Some(card) = find(model, day, overrides) else {
        return Priced::unpriced("unknown_model", None);
    };
    let version = Some(card.version.clone());
    if !tokens.reported() {
        return Priced::unpriced("usage_not_reported", version);
    }
    if card.audio_distinct && tokens.audio_input.unwrap_or(0) > 0 {
        return Priced::unpriced("audio_rate_not_modelled", version);
    }
    let total_input = tokens
        .input
        .unwrap_or(0)
        .saturating_add(tokens.cache_read.unwrap_or(0))
        .saturating_add(tokens.cache_write.unwrap_or(0));
    let rate = match card.long {
        Some((threshold, long)) if total_input > threshold => long,
        _ => card.base,
    };
    match cost(&rate, tokens) {
        Ok(micros) => Priced {
            cost_micros: Some(micros),
            version,
            reason: None,
        },
        Err(reason) => Priced::unpriced(reason, version),
    }
}

/// Provider-specific Go estimate; other providers retain their own standard card.
pub fn price_provider(
    provider: &str,
    model: &str,
    ts_ms: i64,
    tokens: &Tokens,
    overrides: &[Override],
) -> Priced {
    use chrono::{Datelike, Timelike};
    let dt = chrono::DateTime::from_timestamp_millis(ts_ms);
    let day = dt
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    if provider != "opencode_go" {
        return price(model, &day, tokens, overrides);
    }
    let key = normalize_model(model);
    if overrides
        .iter()
        .any(|o| o.model == model || normalize_model(&o.model) == key)
    {
        return price(model, &day, tokens, overrides);
    }
    let mut estimate = price(&format!("opencode_go/{key}"), &day, tokens, overrides);
    if key.starts_with("deepseek-")
        && dt.is_some_and(|d| {
            d.weekday().num_days_from_monday() < 5
                && ((1..4).contains(&d.hour()) || (6..10).contains(&d.hour()))
        })
    {
        estimate.cost_micros = estimate.cost_micros.and_then(|v| v.checked_mul(2));
        estimate.version = estimate.version.map(|v| format!("{v}-go-peak"));
    }
    estimate
}

/// `tokens × rate` summed over dimensions, in micro-USD (rounded half up). Fails when a
/// dimension with tokens is unknown or has no published price.
fn cost(rate: &Rate, t: &Tokens) -> Result<u64, &'static str> {
    // Each term is tokens × (micro-USD per 1M tokens); the sum is divided by 1M once.
    let mut numerator: u128 = 0;
    let mut add = |tokens: Option<u64>, price: Option<u64>| -> Result<(), &'static str> {
        let tokens = tokens.ok_or("usage_incomplete")?;
        if tokens == 0 {
            return Ok(());
        }
        let price = price.ok_or("rate_not_published")?;
        numerator = numerator
            .checked_add(u128::from(tokens) * u128::from(price))
            .ok_or("overflow")?;
        Ok(())
    };
    add(t.input, rate.input)?;
    add(t.output, rate.output)?;
    add(t.cache_read, rate.cache_read)?;
    match (t.cache_write_5m, t.cache_write_1h) {
        (Some(w5), Some(w1)) if w5.checked_add(w1) == t.cache_write => {
            add(Some(w5), rate.cache_write_5m.or(rate.cache_write))?;
            add(Some(w1), rate.cache_write_1h)?;
        }
        _ => add(t.cache_write, rate.cache_write)?,
    }
    u64::try_from((numerator + 500_000) / 1_000_000).map_err(|_| "overflow")
}

/// API list-price estimate (micro-USD) for `tokens` on `model` on UTC day `day`, honouring user
/// overrides; `None` when unpriced. For collectors pricing native usage.
pub fn estimate_micros(
    store: &crate::store::Store,
    model: &str,
    day: &str,
    tokens: &Tokens,
) -> Option<u64> {
    price(model, day, tokens, &store.list(OVERRIDE_KIND)).cost_micros
}

/// Formats micro-USD as a decimal string with six places (`"1.250000"`).
pub fn usd_string(micros: u64) -> String {
    format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000)
}
/// Formats a rate (micro-USD per MTok) as the shortest exact decimal (`"0.125"`, `"4"`).
pub fn rate_string(micros: u64) -> String {
    let whole = micros / 1_000_000;
    let frac = micros % 1_000_000;
    if frac == 0 {
        return whole.to_string();
    }
    let digits = format!("{frac:06}");
    format!("{whole}.{}", digits.trim_end_matches('0'))
}
/// Parses a non-negative decimal (string or JSON number) with at most 6 decimal places into
/// micro-units. Rejects anything that is not an exact, bounded decimal.
pub fn parse_rate(v: &Value) -> Result<Option<u64>, &'static str> {
    let s = match v {
        Value::Null => return Ok(None),
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return Err("rates must be decimal strings, numbers or null"),
    };
    let (whole, frac) = s.split_once('.').unwrap_or((&s, ""));
    let valid = !whole.is_empty()
        && whole.len() <= 6
        && whole.bytes().all(|b| b.is_ascii_digit())
        && frac.len() <= 6
        && frac.bytes().all(|b| b.is_ascii_digit())
        && !(s.contains('.') && frac.is_empty());
    if !valid {
        return Err("rates must be non-negative decimals with at most 6 decimal places");
    }
    let micros = whole.parse::<u64>().map_err(|_| "invalid rate")? * 1_000_000
        + format!("{frac:0<6}")
            .parse::<u64>()
            .map_err(|_| "invalid rate")?;
    if micros > MAX_RATE_MICROS {
        return Err("rates must be at most 10000 USD per million tokens");
    }
    Ok(Some(micros))
}

fn rate_json(r: &Rate) -> Value {
    let s = |v: Option<u64>| v.map(rate_string);
    json!({
        "input": s(r.input),
        "output": s(r.output),
        "cache_read": s(r.cache_read),
        "cache_write": s(r.cache_write),
        "cache_write_5m": s(r.cache_write_5m),
        "cache_write_1h": s(r.cache_write_1h),
    })
}
/// Public JSON for a card.
pub fn card_json(c: &Card) -> Value {
    json!({
        "model": c.model,
        "provider": if c.provider.is_empty() { Value::Null } else { json!(c.provider) },
        "origin": c.origin,
        "version": c.version,
        "usd_per_mtok": rate_json(&c.base),
        "long_context": c.long.map(|(threshold, r)| json!({"above_input_tokens": threshold, "usd_per_mtok": rate_json(&r)})),
        "effective_from": c.from_day,
        "effective_until": c.until_day,
        "audio_input_priced_separately": c.audio_distinct,
        "note": c.note,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn go_rates_are_provider_scoped_and_follow_utc_peak_boundaries() {
        let ts = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .timestamp_millis()
        };
        let t = Tokens {
            input: Some(1_000_000),
            output: Some(1_000_000),
            cache_read: Some(0),
            cache_write: Some(0),
            ..Default::default()
        };
        for (time, expected) in [
            ("2026-10-02T00:59:59Z", 750_000),
            ("2026-10-02T01:00:00Z", 1_500_000),
            ("2026-10-02T04:00:00Z", 750_000),
            ("2026-10-02T06:00:00Z", 1_500_000),
            ("2026-10-02T10:00:00Z", 750_000),
            ("2026-10-03T01:00:00Z", 750_000),
        ] {
            assert_eq!(
                price_provider("opencode_go", "deepseek-v4-flash", ts(time), &t, &[]).cost_micros,
                Some(expected)
            );
        }
        assert_eq!(
            price_provider("openai", "glm-5.3", ts("2026-10-03T00:00:00Z"), &t, &[]).cost_micros,
            None
        );
        assert_eq!(
            price_provider(
                "opencode_go",
                "glm-5.3",
                ts("2026-10-03T00:00:00Z"),
                &t,
                &[]
            )
            .cost_micros,
            Some(5_800_000)
        );
        let override_card = Override {
            model: "deepseek-v4-flash".into(),
            rate: Rate {
                input: Some(1_000_000),
                output: Some(1_000_000),
                cache_read: Some(0),
                cache_write: Some(0),
                ..Default::default()
            },
            updated_at: "test".into(),
        };
        assert_eq!(
            price_provider(
                "opencode_go",
                "deepseek-v4-flash",
                ts("2026-10-02T01:00:00Z"),
                &t,
                &[override_card]
            )
            .cost_micros,
            Some(2_000_000)
        );
    }

    use super::*;

    fn tokens(input: u64, read: u64, write: u64, output: u64) -> Tokens {
        Tokens {
            input: Some(input),
            cache_read: Some(read),
            cache_write: Some(write),
            output: Some(output),
            reasoning: Some(0),
            ..Tokens::default()
        }
    }

    #[test]
    fn normalizes_snapshots_and_prefixes() {
        assert_eq!(
            normalize_model("claude-haiku-4-5-20251001"),
            "claude-haiku-4-5"
        );
        assert_eq!(normalize_model("models/gemini-2.5-pro"), "gemini-2.5-pro");
        assert_eq!(
            normalize_model("Anthropic/Claude-Opus-5-5"),
            "claude-opus-5-5"
        );
        assert_eq!(normalize_model("gpt-6.1-sol"), "gpt-6.1-sol");
    }

    #[test]
    fn opus_5_5_uses_its_published_cache_read_multiplier() {
        // 1M uncached input $4 + 1M cache read $0.20 + 1M 5m write $5 + 1M output $20.
        let t = tokens(M, M, M, M);
        let p = price("claude-opus-5-5", "2026-10-03", &t, &[]);
        assert_eq!(p.cost_micros, Some(29_200_000));
        assert_eq!(p.version.as_deref(), Some(OFFICIAL_VERSION));
    }

    #[test]
    fn anthropic_ttl_breakdown_prices_one_hour_writes() {
        let mut t = tokens(0, 0, 3 * M, 0);
        t.cache_write_5m = Some(M);
        t.cache_write_1h = Some(2 * M);
        // Sonnet 5.5: 1M × $2.50 + 2M × $4.
        let p = price("claude-sonnet-5-5", "2026-10-03", &t, &[]);
        assert_eq!(p.cost_micros, Some(10_500_000));
    }

    #[test]
    fn openai_long_context_applies_to_the_whole_request() {
        // 300K uncached + 0 cached > 272K: long rates $4 in / $15 out for gpt-6.1-sol.
        let t = tokens(300_000, 0, 0, 1000);
        let p = price("gpt-6.1-sol", "2026-10-03", &t, &[]);
        assert_eq!(p.cost_micros, Some(1_215_000));
        let short = price("gpt-6.1-sol", "2026-10-03", &tokens(272_000, 0, 0, 0), &[]);
        assert_eq!(short.cost_micros, Some(544_000));
    }

    #[test]
    fn gemini_prices_change_on_the_published_date() {
        let t = tokens(M, 0, 0, M);
        let before = price("gemini-3.8-flash", "2026-12-31", &t, &[]);
        let after = price("gemini-3.8-flash", "2027-01-01", &t, &[]);
        assert_eq!(before.cost_micros, Some(4_500_000));
        assert_eq!(after.cost_micros, Some(9_000_000));
    }

    #[test]
    fn unknown_models_and_missing_rates_are_unpriced_not_zero() {
        let t = tokens(10, 0, 0, 10);
        let p = price("my-local-model", "2026-10-03", &t, &[]);
        assert_eq!((p.cost_micros, p.reason), (None, Some("unknown_model")));
        // gpt-5.5-pro publishes no cached-input price.
        let p = price("gpt-5.5-pro", "2026-10-03", &tokens(10, 5, 0, 10), &[]);
        assert_eq!(
            (p.cost_micros, p.reason),
            (None, Some("rate_not_published"))
        );
        // Unknown cache dimension.
        let mut t = tokens(10, 0, 0, 10);
        t.cache_read = None;
        let p = price("gpt-6.1-sol", "2026-10-03", &t, &[]);
        assert_eq!(p.reason, Some("usage_incomplete"));
    }

    #[test]
    fn overrides_win_and_are_versioned() {
        let o = Override {
            model: "my-local-model".into(),
            rate: Rate {
                input: Some(1_250_000),
                output: Some(10 * M),
                ..Rate::default()
            },
            updated_at: "2026-10-03T00:00:00Z".into(),
        };
        let p = price("my-local-model", "2026-10-03", &tokens(M, 0, 0, M), &[o]);
        assert_eq!(p.cost_micros, Some(11_250_000));
        assert_eq!(p.version.as_deref(), Some("override-2026-10-03T00:00:00Z"));
    }

    #[test]
    fn rate_strings_round_trip_exactly() {
        assert_eq!(parse_rate(&json!("0.125")), Ok(Some(125_000)));
        assert_eq!(parse_rate(&json!(4)), Ok(Some(4 * M)));
        assert_eq!(rate_string(125_000), "0.125");
        assert_eq!(rate_string(4 * M), "4");
        assert_eq!(usd_string(1_234_567), "1.234567");
        assert!(parse_rate(&json!("-1")).is_err());
        assert!(parse_rate(&json!("1.0000001")).is_err());
        assert!(parse_rate(&json!("1e3")).is_err());
        assert!(parse_rate(&json!("10001")).is_err());
    }
}
