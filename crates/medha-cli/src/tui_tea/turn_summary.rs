//! A finished turn's use and cost in one line, as the desktop shows it.

use protocol::{CostSource, TurnSummary};

fn duration(seconds: f64) -> String {
    let whole = seconds.round() as u64;
    match whole {
        0..60 => format!("{whole}s"),
        60..3600 => format!("{}m {}s", whole / 60, whole % 60),
        _ => format!("{}h {}m", whole / 3600, whole % 3600 / 60),
    }
}

pub(super) fn tokens(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..10_000 => format!("{:.1}k", count as f64 / 1_000.0).replace(".0k", "k"),
        10_000..1_000_000 => format!("{}k", count / 1_000),
        _ => format!("{:.1}M", count as f64 / 1_000_000.0).replace(".0M", "M"),
    }
}

fn dollars(cost: f64) -> String {
    if cost < 0.01 {
        format!("${cost:.4}")
    } else {
        format!("${cost:.2}")
    }
}

/// For example `4m 12s · 9 calls · 182k in (151k cached) · 6k out · $0.41 list price`.
pub(super) fn line(summary: &TurnSummary) -> String {
    let mut parts = vec![
        duration(summary.seconds),
        match summary.calls {
            1 => "1 call".to_owned(),
            calls => format!("{calls} calls"),
        },
    ];
    match summary.side_calls {
        0 => {}
        1 => parts.push("1 side call".to_owned()),
        side => parts.push(format!("{side} side calls")),
    }
    let input = tokens(summary.input_tokens);
    parts.push(match summary.cached_tokens {
        0 => format!("{input} in"),
        cached => format!("{input} in ({} cached)", tokens(cached)),
    });
    parts.push(format!("{} out", tokens(summary.output_tokens)));
    parts.push(match (summary.cost_usd, summary.priced) {
        (Some(cost), CostSource::Own) => dollars(cost),
        (Some(cost), CostSource::List) => format!("{} list price", dollars(cost)),
        (Some(cost), CostSource::Partly) => format!("{} for the priced calls", dollars(cost)),
        _ => "not priced".to_owned(),
    });
    parts.join(" · ")
}
