use std::collections::HashMap;

pub struct Template {
    pub name: &'static str,
    pub system: &'static str,
    pub user: &'static str,
}

pub const DEFAULTS: &[Template] = &[
    Template {
        name: "product_description",
        system: "You are a helpful marketing copywriter.",
        user: "Write a concise product description for: {{product}}. Target audience: {{audience}}.",
    },
    Template {
        name: "broadcast_draft",
        system: "You write engaging broadcast messages.",
        user: "Draft a short broadcast message about: {{topic}}. Tone: {{tone}}.",
    },
    Template {
        name: "reply_suggestion",
        system: "You suggest helpful replies.",
        user: "Suggest a reply to this customer message: {{message}}",
    },
];

pub fn render(
    tpl: &str,
    vars: &HashMap<String, String>,
) -> Result<String, crate::ai::error::AiError> {
    let mut out = tpl.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{}}}}}", k), v);
    }
    // check missing
    // simple: find {{...}}
    let mut i = 0;
    while let Some(start) = out[i..].find("{{") {
        let abs = i + start;
        if let Some(end) = out[abs..].find("}}") {
            let var = &out[abs + 2..abs + end].trim().to_string();
            if !vars.contains_key(var) {
                return Err(crate::ai::error::AiError::TemplateVarMissing(var.clone()));
            }
            i = abs + end + 2;
        } else {
            break;
        }
    }
    Ok(out)
}

pub fn find(name: &str) -> Option<&'static Template> {
    DEFAULTS.iter().find(|t| t.name == name)
}
