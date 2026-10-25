use std::collections::HashMap;
use std::sync::Arc;

use crate::ai::cost;
use crate::ai::error::AiError;
use crate::ai::provider::{AiProvider, GenerateRequest, Message};
use crate::ai::rate_limit::RateLimiter;
use crate::ai::templates;
use crate::db::Db;

pub struct AiService {
    pub provider: Arc<dyn AiProvider>,
    pub model: String,
    pub db: Arc<Db>,
    pub rate_limiter: RateLimiter,
    pub daily_token_budget_global: Option<i64>,
    pub enabled: bool,
}

impl AiService {
    pub async fn generate(
        &self,
        seller_id: i64,
        user_id: Option<i64>,
        template_name: &str,
        vars: HashMap<String, String>,
        max_tokens: i64,
    ) -> Result<String, AiError> {
        if !self.enabled {
            return Err(AiError::Disabled);
        }

        // rate limit
        if let Err(retry) = self.rate_limiter.check(seller_id) {
            return Err(AiError::RateLimited {
                retry_after_secs: retry,
            });
        }

        // template
        let tpl = templates::find(template_name)
            .ok_or_else(|| AiError::TemplateNotFound(template_name.into()))?;
        let system = templates::render(tpl.system, &vars)?;
        let user_prompt = templates::render(tpl.user, &vars)?;

        // budget check
        let used_today = self.db.ai_usage_today_tokens(seller_id);
        let per_seller_budget = self.db.get_seller_ai_settings(seller_id).and_then(|s| s.2);
        let limit = per_seller_budget.or(self.daily_token_budget_global);
        // estimate: assume max_tokens completion + rough prompt
        let est = max_tokens + 500;
        if let Err((used, lim)) = cost::check_budget(used_today, limit, est) {
            return Err(AiError::BudgetExceeded { used, limit: lim });
        }

        let msgs = vec![
            Message {
                role: "system".into(),
                content: system,
            },
            Message {
                role: "user".into(),
                content: user_prompt,
            },
        ];
        let req = GenerateRequest {
            model: self.model.clone(),
            messages: msgs,
            max_tokens,
            temperature: 0.7,
        };
        let resp = self.provider.generate(req).await?;
        let _ = cost::estimate_cost(
            &self.model,
            resp.usage.prompt_tokens,
            resp.usage.completion_tokens,
        );

        // record usage
        self.db.record_ai_usage(
            seller_id,
            user_id,
            self.provider.name(),
            &self.model,
            Some(template_name),
            resp.usage.prompt_tokens,
            resp.usage.completion_tokens,
            resp.usage.total_tokens,
            0.0,
            "ok",
            None,
        );

        Ok(resp.content)
    }
}
