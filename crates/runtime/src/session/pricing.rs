use super::*;

pub(super) fn warn_unknown(
    lock: &lockfile::MedhaLock,
    registered_tools: &std::collections::HashSet<String>,
    notices: &dyn Notices,
) {
    let stale = unknown_approvals(&lock.policy.approve, registered_tools);
    if !stale.is_empty() {
        notices.say(&format!(
            "warning: [policy].approve names {} tool(s) that do not exist: {} — \
             they gate nothing; update medha.lock",
            stale.len(),
            stale.join(", ")
        ));
    }
}

pub(super) fn resolve(
    lock: &lockfile::MedhaLock,
    model_name: &str,
    catalogue_pricing: Option<(f64, f64, Option<f64>, Option<f64>)>,
    base_budget: &kernel::Budget,
    notices: &dyn Notices,
) -> Option<kernel::Pricing> {
    // models.dev prices are advisory for self-hosted routes.
    let pricing = match (lock.pricing.input_per_mtok, lock.pricing.output_per_mtok) {
        (Some(i), Some(o)) => Some(kernel::Pricing {
            input_per_mtok: i,
            output_per_mtok: o,
            // Configured rates are the operator's own; nothing is inferred for
            // cached reads, so they bill at the input rate unless stated.
            cached_input_per_mtok: lock.pricing.cached_input_per_mtok,
            cache_write_per_mtok: lock.pricing.cache_write_per_mtok,
            indicative: false,
        }),
        _ => catalogue_pricing.map(|(input, output, cached, written)| kernel::Pricing {
            input_per_mtok: input,
            output_per_mtok: output,
            cached_input_per_mtok: cached,
            cache_write_per_mtok: written,
            indicative: true,
        }),
    };
    match &pricing {
        Some(p) if p.indicative => notices.say(&format!(
            "cost meter: {model_name} list price from models.dev (${:.2}/M in, ${:.2}/M out) — \
             indicative only; set [pricing] in medha.lock for your real rate",
            p.input_per_mtok, p.output_per_mtok
        )),
        Some(_) => {}
        None => {
            if base_budget.max_cost_usd.is_some() {
                notices.say(&format!(
                    "warning: max_cost_usd is set but no pricing is known for '{model_name}' — \
                     model requests will be refused. Set [pricing] input_per_mtok / \
                     output_per_mtok in medha.lock."
                ));
            }
        }
    }
    pricing
}
