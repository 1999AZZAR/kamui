//! Optional per-model cost estimation for `/stats` and `/usage`.
//!
//! Every usage row already records the model and its input/output token counts, so estimating cost
//! needs no new storage — only prices, which only the user can supply. Everything here stays inert
//! until a `[pricing]` section exists in `kamui.toml`: with none configured `Prices::is_empty` is
//! true, and the reports print exactly what they printed before cost tracking existed.

use anyhow::{Result, bail};
use serde::Deserialize;
use std::collections::HashMap;

/// Prices are quoted per *million* tokens, the unit every provider's own pricing page uses
/// (OpenAI, Anthropic, OpenRouter, DeepSeek, Groq), so the number can be copied across unchanged.
/// A per-token figure would be the same information written as a string of leading zeroes, which
/// is exactly where an accidental factor of ten hides.
const TOKENS_PER_PRICE_UNIT: f64 = 1_000_000.0;

/// Printed in front of an amount when `[pricing].currency` is not set. Kamui neither knows nor
/// converts exchange rates: this is a display label for whatever currency the user typed prices in.
const DEFAULT_CURRENCY: &str = "$";

/// Shown instead of an amount for usage Kamui cannot price at all. Never a number, because a zero
/// would claim the usage was free.
const UNPRICED: &str = "unpriced";

/// Shown when a row covers no tokens whatsoever, so the column is never blank.
const NO_USAGE: &str = "-";

/// What one model costs per million tokens. Input and output are kept separate because every
/// provider charges more for output; one blended rate would misreport every session.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ModelPrice {
    pub input_per_million: f64,
    pub output_per_million: f64,
}

/// A resolved price table. Keys are model identifiers lowercased for matching.
#[derive(Debug, Clone, Default)]
pub struct Prices {
    currency: Option<String>,
    models: HashMap<String, ModelPrice>,
}

/// The cost of a group of usage rows, with tokens that could not be priced kept separate so a
/// report can say so rather than fold them in as free.
#[derive(Debug, Clone, Copy, Default)]
pub struct CostTally {
    amount: f64,
    priced_tokens: i64,
    unpriced_tokens: i64,
}

impl CostTally {
    /// Whether any tokens in this tally came from a model with no configured price.
    pub fn has_unpriced(&self) -> bool {
        self.unpriced_tokens > 0
    }
}

impl Prices {
    /// Build a table from configured entries. A price that cannot be a real price is rejected
    /// rather than clamped: a wrong number in a money report is worse than a startup error.
    pub fn new(
        currency: Option<String>,
        entries: impl IntoIterator<Item = (String, ModelPrice)>,
    ) -> Result<Self> {
        let mut models = HashMap::new();
        for (model, price) in entries {
            let key = model.trim().to_lowercase();
            if key.is_empty() {
                bail!("[pricing.models] contains an entry with an empty model name");
            }
            for (field, value) in [
                ("input_per_million", price.input_per_million),
                ("output_per_million", price.output_per_million),
            ] {
                if !value.is_finite() || value < 0.0 {
                    bail!("[pricing.models.\"{model}\"] {field} must be a non-negative number");
                }
            }
            models.insert(key, price);
        }
        let currency = currency.filter(|symbol| !symbol.trim().is_empty());
        Ok(Self { currency, models })
    }

    /// True when the user configured no prices at all. Callers use this to leave cost out of a
    /// report entirely instead of printing an empty column.
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    fn symbol(&self) -> &str {
        self.currency.as_deref().unwrap_or(DEFAULT_CURRENCY)
    }

    /// The configured price for a model, matched on the exact identifier stored with the usage row
    /// (case- and padding-insensitive, but no wildcards — a price applies to the model it names).
    /// A `None` model, which is what rows written before `user_version = 5` carry, never matches.
    fn price_for(&self, model: Option<&str>) -> Option<&ModelPrice> {
        self.models.get(model?.trim().to_lowercase().as_str())
    }

    /// Add up the cost of `(model, input_tokens, output_tokens)` rows, tracking tokens from
    /// unpriced models separately so the result can be reported honestly.
    pub fn tally<'a>(
        &self,
        rows: impl IntoIterator<Item = (Option<&'a str>, i64, i64)>,
    ) -> CostTally {
        let mut tally = CostTally::default();
        for (model, input, output) in rows {
            match self.price_for(model) {
                Some(price) => {
                    tally.amount += (input as f64 * price.input_per_million
                        + output as f64 * price.output_per_million)
                        / TOKENS_PER_PRICE_UNIT;
                    tally.priced_tokens += input + output;
                }
                None => tally.unpriced_tokens += input + output,
            }
        }
        tally
    }

    /// Render a tally as a report cell. Usage that could not be priced at all reads `unpriced`,
    /// and a partly priced total carries a trailing `+`, so an amount never silently stands for
    /// more spend than it covers. Four decimals, because a single turn often costs under a cent.
    pub fn format(&self, tally: &CostTally) -> String {
        if tally.priced_tokens == 0 {
            return if tally.unpriced_tokens == 0 {
                NO_USAGE.to_string()
            } else {
                UNPRICED.to_string()
            };
        }
        let marker = if tally.has_unpriced() { "+" } else { "" };
        format!("{}{:.4}{marker}", self.symbol(), tally.amount)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(input: f64, output: f64) -> ModelPrice {
        ModelPrice {
            input_per_million: input,
            output_per_million: output,
        }
    }

    fn table(entries: Vec<(&str, ModelPrice)>) -> Prices {
        Prices::new(
            None,
            entries
                .into_iter()
                .map(|(model, price)| (model.to_string(), price)),
        )
        .unwrap()
    }

    #[test]
    fn prices_are_per_million_tokens_with_separate_input_and_output_rates() {
        let prices = table(vec![("gpt-4o", price(2.5, 10.0))]);

        // 1M input at 2.50 plus 500k output at 10.00 = 2.50 + 5.00.
        let tally = prices.tally([(Some("gpt-4o"), 1_000_000, 500_000)]);

        assert!((tally.amount - 7.5).abs() < 1e-9);
        assert_eq!(prices.format(&tally), "$7.5000");
    }

    #[test]
    fn no_configured_prices_is_reported_as_empty() {
        assert!(Prices::default().is_empty());
        assert!(Prices::new(None, []).unwrap().is_empty());
        assert!(!table(vec![("m", price(1.0, 1.0))]).is_empty());
    }

    #[test]
    fn a_model_without_a_price_reads_unpriced_rather_than_free() {
        let prices = table(vec![("gpt-4o", price(2.5, 10.0))]);

        let tally = prices.tally([(Some("codeqwen:latest"), 1_000, 1_000)]);

        assert!(tally.has_unpriced());
        assert_eq!(prices.format(&tally), "unpriced");
    }

    #[test]
    fn usage_recorded_without_a_model_is_unpriced() {
        let prices = table(vec![("gpt-4o", price(2.5, 10.0))]);

        let tally = prices.tally([(None, 1_000, 1_000)]);

        assert_eq!(prices.format(&tally), "unpriced");
    }

    #[test]
    fn a_partly_priced_total_is_marked_so_the_amount_is_not_mistaken_for_everything() {
        let prices = table(vec![("gpt-4o", price(1.0, 1.0))]);

        let tally = prices.tally([
            (Some("gpt-4o"), 1_000_000, 0),
            (Some("mystery-model"), 4_000_000, 0),
        ]);

        assert!(tally.has_unpriced());
        assert_eq!(prices.format(&tally), "$1.0000+");
    }

    #[test]
    fn an_explicit_zero_price_is_free_not_unpriced() {
        let prices = table(vec![("local", price(0.0, 0.0))]);

        let tally = prices.tally([(Some("local"), 9_000, 9_000)]);

        assert!(!tally.has_unpriced());
        assert_eq!(prices.format(&tally), "$0.0000");
    }

    #[test]
    fn model_matching_ignores_case_and_surrounding_space() {
        let prices = table(vec![("  GPT-4o  ", price(1.0, 1.0))]);

        let tally = prices.tally([(Some("gpt-4O"), 1_000_000, 0)]);

        assert_eq!(prices.format(&tally), "$1.0000");
    }

    #[test]
    fn an_empty_tally_shows_no_amount() {
        let prices = table(vec![("m", price(1.0, 1.0))]);

        assert_eq!(prices.format(&prices.tally([])), "-");
    }

    #[test]
    fn the_currency_label_is_configurable_and_never_converted() {
        let prices =
            Prices::new(Some("€".to_string()), [("m".to_string(), price(1.0, 1.0))]).unwrap();

        assert_eq!(
            prices.format(&prices.tally([(Some("m"), 1_000_000, 0)])),
            "€1.0000"
        );
    }

    #[test]
    fn a_blank_currency_falls_back_to_the_default_symbol() {
        let prices =
            Prices::new(Some("  ".to_string()), [("m".to_string(), price(1.0, 1.0))]).unwrap();

        assert_eq!(
            prices.format(&prices.tally([(Some("m"), 1_000_000, 0)])),
            "$1.0000"
        );
    }

    #[test]
    fn impossible_prices_are_rejected() {
        let negative = Prices::new(None, [("m".to_string(), price(-1.0, 1.0))]);
        assert!(
            negative
                .unwrap_err()
                .to_string()
                .contains("input_per_million")
        );

        let infinite = Prices::new(None, [("m".to_string(), price(1.0, f64::INFINITY))]);
        assert!(
            infinite
                .unwrap_err()
                .to_string()
                .contains("output_per_million")
        );

        let nameless = Prices::new(None, [("   ".to_string(), price(1.0, 1.0))]);
        assert!(
            nameless
                .unwrap_err()
                .to_string()
                .contains("empty model name")
        );
    }
}
