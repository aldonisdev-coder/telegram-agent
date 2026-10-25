pub struct Pricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
}

pub fn pricing(model: &str) -> Pricing {
    // rough defaults
    match model {
        m if m.contains("gpt-4o-mini") => Pricing {
            input_per_million: 0.15,
            output_per_million: 0.60,
        },
        m if m.contains("gpt-4o") => Pricing {
            input_per_million: 2.50,
            output_per_million: 10.00,
        },
        m if m.contains("claude-3-5-sonnet") => Pricing {
            input_per_million: 3.00,
            output_per_million: 15.00,
        },
        m if m.contains("claude-3-haiku") => Pricing {
            input_per_million: 0.25,
            output_per_million: 1.25,
        },
        _ => Pricing {
            input_per_million: 1.00,
            output_per_million: 3.00,
        },
    }
}

pub fn estimate_cost(model: &str, prompt_tokens: i64, completion_tokens: i64) -> f64 {
    let p = pricing(model);
    (prompt_tokens as f64 / 1_000_000.0) * p.input_per_million
        + (completion_tokens as f64 / 1_000_000.0) * p.output_per_million
}

pub fn check_budget(
    used_today: i64,
    limit: Option<i64>,
    requested_estimate: i64,
) -> Result<(), (i64, i64)> {
    if let Some(lim) = limit
        && used_today + requested_estimate > lim
    {
        return Err((used_today, lim));
    }
    Ok(())
}
