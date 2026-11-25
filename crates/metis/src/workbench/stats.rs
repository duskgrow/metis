//! Independent Activity, Usage, and Context projections and their display math.

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct StatsInput {
    pub(super) activity: Option<ActivityInput>,
    pub(super) usage: Option<UsageInput>,
    pub(super) context: Option<ContextInput>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct ActivityInput {
    pub(super) turns: i64,
    pub(super) steps: i64,
    pub(super) llm_ms: i64,
    pub(super) tool_ms: i64,
    pub(super) ttft_ms: i64,
    pub(super) ttft_steps: i64,
    pub(super) decode_ms: i64,
    pub(super) decode_tokens: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct UsageInput {
    pub(super) uncached_input: i64,
    pub(super) cache_read: i64,
    pub(super) cache_write: i64,
    pub(super) output: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct ContextInput {
    pub(super) context_window: Option<i64>,
    pub(super) projected_tokens: i64,
    pub(super) breakdown: Option<ContextBreakdown>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct ContextBreakdown {
    pub(super) system: i64,
    pub(super) tools: i64,
    pub(super) messages: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct StatsProjection {
    pub(super) activity: Option<ActivityProjection>,
    pub(super) usage: Option<UsageProjection>,
    pub(super) context: Option<ContextProjection>,
}

impl StatsProjection {
    pub(super) fn project(input: StatsInput) -> Self {
        Self {
            activity: input.activity.map(ActivityProjection::project),
            usage: input.usage.and_then(UsageProjection::project),
            context: input.context.and_then(ContextProjection::project),
        }
    }

    pub(super) fn has_visible_domain(&self) -> bool {
        self.activity.is_some() || self.usage.is_some() || self.context.is_some()
    }
}

/// Whole-Session execution activity.
///
/// Negative inputs are clamped to zero. `avgTTFT = ttftMs / ttftSteps` only
/// when `ttftSteps > 0`; `speed = decodeTokens / (decodeMs / 1000)` only when
/// `decodeMs > 0`. A zero denominator produces no rate, never zero, NaN, or
/// infinity.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ActivityProjection {
    pub(super) turns: u64,
    pub(super) steps: u64,
    pub(super) llm_ms: u64,
    pub(super) tool_ms: u64,
    pub(super) avg_ttft_ms: Option<f64>,
    pub(super) decode_tokens_per_second: Option<f64>,
}

#[allow(clippy::cast_precision_loss, clippy::needless_pass_by_value)]
impl ActivityProjection {
    fn project(input: ActivityInput) -> Self {
        let ttft_ms = nonnegative(input.ttft_ms);
        let ttft_steps = nonnegative(input.ttft_steps);
        let decode_ms = nonnegative(input.decode_ms);
        let decode_tokens = nonnegative(input.decode_tokens);
        let avg_ttft_ms = (ttft_steps > 0).then(|| ttft_ms as f64 / ttft_steps as f64);
        let decode_tokens_per_second =
            (decode_ms > 0).then(|| decode_tokens as f64 / (decode_ms as f64 / 1000.0));

        Self {
            turns: nonnegative(input.turns),
            steps: nonnegative(input.steps),
            llm_ms: nonnegative(input.llm_ms),
            tool_ms: nonnegative(input.tool_ms),
            avg_ttft_ms,
            decode_tokens_per_second,
        }
    }

    pub(super) fn llm_duration(&self) -> String {
        format_duration_ms(self.llm_ms as f64)
    }

    pub(super) fn tool_duration(&self) -> String {
        format_duration_ms(self.tool_ms as f64)
    }

    pub(super) fn average_ttft_duration(&self) -> Option<String> {
        self.avg_ttft_ms.map(format_duration_ms)
    }

    pub(super) fn decode_speed(&self) -> Option<String> {
        self.decode_tokens_per_second.map(format_decode_speed)
    }
}

/// Token-only Usage. No Activity or Context value contributes to these fields.
///
/// `billedInput = uncachedInput + cacheRead + cacheWrite` and
/// `total = billedInput + output`. `cacheHit = cacheRead / billedInput` only
/// when `billedInput > 0`; zero total hides Usage entirely.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct UsageProjection {
    pub(super) billed_input: u64,
    pub(super) total: u64,
    pub(super) cache_hit: Option<f64>,
    pub(super) cache_hit_percent: Option<String>,
    pub(super) compact_total: String,
}

#[allow(clippy::cast_precision_loss, clippy::needless_pass_by_value)]
impl UsageProjection {
    fn project(input: UsageInput) -> Option<Self> {
        let uncached_input = nonnegative(input.uncached_input);
        let cache_read = nonnegative(input.cache_read);
        let cache_write = nonnegative(input.cache_write);
        let output = nonnegative(input.output);
        let billed_input = uncached_input
            .saturating_add(cache_read)
            .saturating_add(cache_write);
        let total = billed_input.saturating_add(output);
        if total == 0 {
            return None;
        }

        let cache_hit = (billed_input > 0).then(|| cache_read as f64 / billed_input as f64);
        let cache_hit_percent =
            (billed_input > 0).then(|| format_cache_hit_percent(cache_read, billed_input));
        Some(Self {
            billed_input,
            total,
            cache_hit,
            cache_hit_percent,
            compact_total: format_compact_total(total),
        })
    }
}

/// Next-request Context estimate.
///
/// For a nonzero `window`,
/// `percent = min(100, round(projectedTokens / window * 100))` and
/// `breakdownTotal = system + tools + messages`. With a nonzero
/// breakdown, each classified width is `percent * bucket / breakdownTotal` and
/// `remaining = 100 - percent`. Missing/zero breakdown instead produces one
/// unclassified width of `percent`. Zero percent produces no fill segments;
/// missing/zero window hides Context.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ContextProjection {
    pub(super) percent: u32,
    pub(super) remaining_percent: u32,
    pub(super) segments: Vec<ContextSegment>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ContextSegment {
    pub(super) kind: ContextSegmentKind,
    pub(super) width_percent: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ContextSegmentKind {
    System,
    Tools,
    Messages,
    Unclassified,
}

#[allow(clippy::cast_precision_loss)]
impl ContextProjection {
    fn project(input: ContextInput) -> Option<Self> {
        let window = nonnegative(input.context_window?);
        if window == 0 {
            return None;
        }

        let projected_tokens = nonnegative(input.projected_tokens);
        let numerator = u128::from(projected_tokens) * 100;
        let denominator = u128::from(window);
        let percent = (numerator / denominator
            + u128::from((numerator % denominator) * 2 >= denominator))
        .min(100) as u32;
        let remaining_percent = 100 - percent;
        let breakdown_total = input.breakdown.as_ref().map_or(0, |breakdown| {
            nonnegative(breakdown.system)
                .saturating_add(nonnegative(breakdown.tools))
                .saturating_add(nonnegative(breakdown.messages))
        });
        let segments = match input.breakdown {
            _ if percent == 0 => Vec::new(),
            Some(breakdown) if breakdown_total > 0 => [
                (ContextSegmentKind::System, breakdown.system),
                (ContextSegmentKind::Tools, breakdown.tools),
                (ContextSegmentKind::Messages, breakdown.messages),
            ]
            .into_iter()
            .map(|(kind, value)| ContextSegment {
                kind,
                width_percent: f64::from(percent) * nonnegative(value) as f64
                    / breakdown_total as f64,
            })
            .collect(),
            _ => vec![ContextSegment {
                kind: ContextSegmentKind::Unclassified,
                width_percent: f64::from(percent),
            }],
        };

        Some(Self {
            percent,
            remaining_percent,
            segments,
        })
    }
}

fn nonnegative(value: i64) -> u64 {
    u64::try_from(value.max(0)).expect("a nonnegative i64 always fits u64")
}

/// Below 60 seconds, show exactly one decimal; at or above it, round to whole
/// seconds and show integer minutes plus remaining seconds. Inputs are already
/// clamped to nonnegative finite durations, so the display casts cannot wrap.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn format_duration_ms(ms: f64) -> String {
    if ms < 60_000.0 {
        let tenths = (ms / 100.0).round() as u64;
        return format!("{}.{}s", tenths / 10, tenths % 10);
    }

    let seconds = (ms / 1000.0).round() as u64;
    format!("{}m {}s", seconds / 60, seconds % 60)
}

/// Speeds at least 10 are integers; positive lower speeds have at most one
/// decimal, and zero is displayed as zero.
fn format_decode_speed(speed: f64) -> String {
    if speed >= 10.0 {
        return format!("{} token/s", speed.round());
    }
    if speed > 0.0 {
        let rounded = (speed * 10.0).round() / 10.0;
        if rounded >= 10.0 {
            return "10 token/s".to_owned();
        }
        let formatted = format!("{rounded:.1}");
        return format!(
            "{} token/s",
            formatted.strip_suffix(".0").unwrap_or(&formatted)
        );
    }
    "0 token/s".to_owned()
}

/// Raw totals select no suffix below 1000, K below 1000000, and M thereafter.
/// After scaling, values below 100 have at most one decimal; larger values are
/// rounded to integers. Rounding carries within the selected unit.
fn format_compact_total(total: u64) -> String {
    if total < 1000 {
        return total.to_string();
    }

    let (unit, suffix) = if total < 1_000_000 {
        (1000, "K")
    } else {
        (1_000_000, "M")
    };
    if total / unit < 100 {
        let tenths = rounded_div(total, unit / 10);
        if tenths.is_multiple_of(10) {
            format!("{}{suffix}", tenths / 10)
        } else {
            format!("{}.{}{suffix}", tenths / 10, tenths % 10)
        }
    } else {
        format!("{}{suffix}", rounded_div(total, unit))
    }
}

fn rounded_div(value: u64, divisor: u64) -> u64 {
    value / divisor + u64::from(value % divisor >= divisor / 2)
}

/// Percentages are rounded half-up. When `cacheRead < billedInput`, decimal
/// places increase until the rendered number is strictly below 100%.
fn format_cache_hit_percent(cache_read: u64, billed_input: u64) -> String {
    let mut decimals = 0;
    loop {
        let scale = 10_u128.pow(decimals);
        let rounded = rounded_percentage_units(cache_read, billed_input, decimals);
        if cache_read == billed_input || rounded < 100 * scale {
            return if decimals == 0 {
                format!("{rounded}%")
            } else {
                format!(
                    "{}.{:0width$}%",
                    rounded / scale,
                    rounded % scale,
                    width = decimals as usize
                )
            };
        }
        decimals += 1;
    }
}

fn rounded_percentage_units(cache_read: u64, billed_input: u64, decimals: u32) -> u128 {
    // Long division keeps the false-100% guard exact even when f64 cannot
    // distinguish adjacent u64 token counts.
    let numerator = u128::from(cache_read) * 100;
    let denominator = u128::from(billed_input);
    let whole = numerator / denominator;
    let mut remainder = numerator % denominator;
    let mut fraction = 0_u128;
    for _ in 0..decimals {
        remainder *= 10;
        fraction = fraction * 10 + remainder / denominator;
        remainder %= denominator;
    }
    remainder *= 10;
    let round_up = u128::from(remainder / denominator >= 5);
    whole * 10_u128.pow(decimals) + fraction + round_up
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_clamps_negatives_and_computes_rates() {
        let activity = ActivityProjection::project(ActivityInput {
            turns: -1,
            steps: -2,
            llm_ms: -3,
            tool_ms: 1250,
            ttft_ms: 3000,
            ttft_steps: 2,
            decode_ms: 2000,
            decode_tokens: 30,
        });

        assert_eq!(activity.turns, 0);
        assert_eq!(activity.steps, 0);
        assert_eq!(activity.llm_duration(), "0.0s");
        assert_eq!(activity.tool_duration(), "1.3s");
        assert_eq!(activity.avg_ttft_ms, Some(1500.0));
        assert_eq!(activity.average_ttft_duration().as_deref(), Some("1.5s"));
        assert_eq!(activity.decode_tokens_per_second, Some(15.0));
        assert_eq!(activity.decode_speed().as_deref(), Some("15 token/s"));
    }

    #[test]
    fn activity_zero_denominators_hide_rates_instead_of_faking_zero() {
        let activity = ActivityProjection::project(ActivityInput {
            ttft_ms: 500,
            ttft_steps: 0,
            decode_ms: 0,
            decode_tokens: 100,
            ..ActivityInput::default()
        });

        assert_eq!(activity.avg_ttft_ms, None);
        assert_eq!(activity.average_ttft_duration(), None);
        assert_eq!(activity.decode_tokens_per_second, None);
        assert_eq!(activity.decode_speed(), None);
    }

    #[test]
    fn activity_formatting_honors_rounding_and_unit_boundaries() {
        assert_eq!(format_duration_ms(1250.0), "1.3s");
        assert_eq!(format_duration_ms(59_999.0), "60.0s");
        assert_eq!(format_duration_ms(59_999.6), "60.0s");
        assert_eq!(format_duration_ms(60_000.0), "1m 0s");
        assert_eq!(format_duration_ms(60_500.0), "1m 1s");
        assert_eq!(format_decode_speed(0.0), "0 token/s");
        assert_eq!(format_decode_speed(9.94), "9.9 token/s");
        assert_eq!(format_decode_speed(9.95), "10 token/s");
        assert_eq!(format_decode_speed(10.49), "10 token/s");
        assert_eq!(format_decode_speed(10.5), "11 token/s");
    }

    #[test]
    fn usage_computes_billed_total_and_cache_hit_from_tokens_only() {
        let usage = UsageProjection::project(UsageInput {
            uncached_input: 10,
            cache_read: 20,
            cache_write: 30,
            output: 5,
        })
        .expect("nonzero output keeps Usage visible");

        assert_eq!(usage.billed_input, 60);
        assert_eq!(usage.total, 65);
        assert_eq!(usage.cache_hit, Some(20.0 / 60.0));
        assert_eq!(usage.cache_hit_percent.as_deref(), Some("33%"));
        assert_eq!(usage.compact_total, "65");
    }

    #[test]
    fn usage_hides_zero_total_and_output_only_has_no_cache_summary() {
        assert_eq!(UsageProjection::project(UsageInput::default()), None);
        assert_eq!(
            UsageProjection::project(UsageInput {
                uncached_input: -10,
                cache_read: -20,
                cache_write: -30,
                output: -40,
            }),
            None
        );

        let output_only = UsageProjection::project(UsageInput {
            output: 5,
            ..UsageInput::default()
        })
        .expect("output alone is visible");
        assert_eq!(output_only.billed_input, 0);
        assert_eq!(output_only.cache_hit, None);
        assert_eq!(output_only.cache_hit_percent, None);
        assert_eq!(output_only.compact_total, "5");
    }

    #[test]
    fn usage_never_rounds_a_partial_cache_hit_to_one_hundred_percent() {
        for (cache_read, cache_write, expected) in [
            (999, 1, "99.9%"),
            (9999, 1, "99.99%"),
            (99_999, 1, "99.999%"),
        ] {
            let usage = UsageProjection::project(UsageInput {
                cache_read,
                cache_write,
                ..UsageInput::default()
            })
            .expect("cache tokens keep Usage visible");
            assert_eq!(usage.cache_hit_percent.as_deref(), Some(expected));
        }

        let full_hit = UsageProjection::project(UsageInput {
            cache_read: 100,
            ..UsageInput::default()
        })
        .expect("cache tokens keep Usage visible");
        assert_eq!(full_hit.cache_hit_percent.as_deref(), Some("100%"));

        let adjacent = format_cache_hit_percent(i64::MAX as u64 - 1, i64::MAX as u64);
        assert!(adjacent.starts_with("99."));
        assert_ne!(adjacent, "100%");
    }

    #[test]
    fn compact_usage_covers_k_m_decimal_and_carry_boundaries() {
        for (total, expected) in [
            (0, "0"),
            (999, "999"),
            (1000, "1K"),
            (1050, "1.1K"),
            (99_949, "99.9K"),
            (99_950, "100K"),
            (999_499, "999K"),
            (999_500, "1000K"),
            (999_999, "1000K"),
            (1_000_000, "1M"),
            (1_050_000, "1.1M"),
            (99_999_999, "100M"),
            (999_999_999, "1000M"),
        ] {
            assert_eq!(format_compact_total(total), expected, "total={total}");
        }
    }

    #[test]
    fn context_is_hidden_for_missing_zero_or_negative_window() {
        for context_window in [None, Some(0), Some(-1)] {
            let context = ContextProjection::project(ContextInput {
                context_window,
                projected_tokens: 50,
                breakdown: Some(ContextBreakdown {
                    system: 10,
                    ..ContextBreakdown::default()
                }),
            });
            assert_eq!(context, None);
        }
    }

    #[test]
    fn context_uses_unclassified_fill_for_missing_or_zero_breakdown() {
        for breakdown in [None, Some(ContextBreakdown::default())] {
            let context = ContextProjection::project(ContextInput {
                context_window: Some(100),
                projected_tokens: 25,
                breakdown,
            })
            .expect("a nonzero window shows Context");

            assert_eq!(context.percent, 25);
            assert_eq!(context.remaining_percent, 75);
            assert_eq!(
                context.segments,
                vec![ContextSegment {
                    kind: ContextSegmentKind::Unclassified,
                    width_percent: 25.0,
                }]
            );
        }
    }

    #[test]
    fn context_zero_percent_has_no_segments_even_with_a_breakdown() {
        let context = ContextProjection::project(ContextInput {
            context_window: Some(1000),
            projected_tokens: 1,
            breakdown: Some(ContextBreakdown {
                system: 20,
                tools: 30,
                messages: 50,
            }),
        })
        .expect("a nonzero window shows Context");

        assert_eq!(context.percent, 0);
        assert_eq!(context.remaining_percent, 100);
        assert_eq!(context.segments.len(), 0);
    }

    #[test]
    fn context_scales_classified_widths_and_caps_at_one_hundred() {
        let breakdown = Some(ContextBreakdown {
            system: 20,
            tools: 30,
            messages: 50,
        });
        let partial = ContextProjection::project(ContextInput {
            context_window: Some(100),
            projected_tokens: 25,
            breakdown: breakdown.clone(),
        })
        .expect("a nonzero window shows Context");
        assert_eq!(partial.percent, 25);
        assert_eq!(partial.remaining_percent, 75);
        assert_eq!(
            partial.segments,
            vec![
                ContextSegment {
                    kind: ContextSegmentKind::System,
                    width_percent: 5.0,
                },
                ContextSegment {
                    kind: ContextSegmentKind::Tools,
                    width_percent: 7.5,
                },
                ContextSegment {
                    kind: ContextSegmentKind::Messages,
                    width_percent: 12.5,
                },
            ]
        );

        let capped = ContextProjection::project(ContextInput {
            context_window: Some(100),
            projected_tokens: 150,
            breakdown,
        })
        .expect("a nonzero window shows Context");
        assert_eq!(capped.percent, 100);
        assert_eq!(capped.remaining_percent, 0);
        assert_eq!(
            capped.segments,
            vec![
                ContextSegment {
                    kind: ContextSegmentKind::System,
                    width_percent: 20.0,
                },
                ContextSegment {
                    kind: ContextSegmentKind::Tools,
                    width_percent: 30.0,
                },
                ContextSegment {
                    kind: ContextSegmentKind::Messages,
                    width_percent: 50.0,
                },
            ]
        );
    }

    #[test]
    fn context_negative_values_are_clamped_without_inventing_fill() {
        let context = ContextProjection::project(ContextInput {
            context_window: Some(100),
            projected_tokens: -1,
            breakdown: Some(ContextBreakdown {
                system: -10,
                tools: -20,
                messages: -30,
            }),
        })
        .expect("a nonzero window shows Context");

        assert_eq!(context.percent, 0);
        assert_eq!(context.segments.len(), 0);
    }

    #[test]
    fn domains_and_dock_visibility_are_independent() {
        let empty = StatsProjection::project(StatsInput::default());
        assert!(!empty.has_visible_domain());

        let activity_only = StatsProjection::project(StatsInput {
            activity: Some(ActivityInput::default()),
            usage: Some(UsageInput::default()),
            context: Some(ContextInput::default()),
        });
        assert!(activity_only.activity.is_some());
        assert_eq!(activity_only.usage, None);
        assert_eq!(activity_only.context, None);
        assert!(activity_only.has_visible_domain());

        let usage_only = StatsProjection::project(StatsInput {
            usage: Some(UsageInput {
                output: 1,
                ..UsageInput::default()
            }),
            ..StatsInput::default()
        });
        assert_eq!(usage_only.activity, None);
        assert!(usage_only.usage.is_some());
        assert_eq!(usage_only.context, None);
        assert!(usage_only.has_visible_domain());
    }
}
