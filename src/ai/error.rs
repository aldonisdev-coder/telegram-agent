use std::fmt;

#[derive(Debug)]
pub enum AiError {
    Disabled,
    RateLimited { retry_after_secs: u64 },
    BudgetExceeded { used: i64, limit: i64 },
    TemplateNotFound(String),
    TemplateVarMissing(String),
    Provider(String),
    Parse(String),
}

impl fmt::Display for AiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AiError::Disabled => write!(f, "AI is disabled"),
            AiError::RateLimited { retry_after_secs } => {
                write!(f, "Rate limited; retry in {}s", retry_after_secs)
            }
            AiError::BudgetExceeded { used, limit } => {
                write!(f, "Daily token budget exceeded ({}/{})", used, limit)
            }
            AiError::TemplateNotFound(n) => write!(f, "Template not found: {}", n),
            AiError::TemplateVarMissing(v) => write!(f, "Missing variable: {}", v),
            AiError::Provider(e) => write!(f, "Provider error: {}", e),
            AiError::Parse(e) => write!(f, "Parse error: {}", e),
        }
    }
}

impl std::error::Error for AiError {}
