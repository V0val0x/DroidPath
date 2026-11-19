use std::sync::OnceLock;

use fancy_regex::Regex;
use serde::{Deserialize, Serialize};

use crate::protocol::ShadowUINode;
use crate::shadow_dom::ShadowTreeIndex;

use super::models::AttributedUIAction;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HazardCategory {
    Payment,
    Destructive,
    ExternalSideEffect,
}

impl HazardCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Payment => "payment",
            Self::Destructive => "destructive",
            Self::ExternalSideEffect => "externalSideEffect",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HazardousActionAssessment {
    pub is_hazardous: bool,
    pub category: Option<HazardCategory>,
    pub matched_pattern: Option<String>,
    pub matched_label: Option<String>,
}

impl HazardousActionAssessment {
    pub fn safe() -> Self {
        Self {
            is_hazardous: false,
            category: None,
            matched_pattern: None,
            matched_label: None,
        }
    }

    pub fn reason(&self) -> String {
        if !self.is_hazardous {
            return String::new();
        }
        let kind = self.category.map(|c| c.as_str()).unwrap_or("hazardous");
        if let Some(ref lbl) = self.matched_label {
            if !lbl.is_empty() {
                return format!("Blocked {kind} action '{lbl}'");
            }
        }
        if let Some(ref pat) = self.matched_pattern {
            if !pat.is_empty() {
                return format!("Matched hazardous rule '{pat}'");
            }
        }
        format!("Blocked {kind} action")
    }
}

struct HazardRegexes {
    payment: Regex,
    destructive: Regex,
}

fn hazard_regexes() -> &'static HazardRegexes {
    static RE: OnceLock<HazardRegexes> = OnceLock::new();
    RE.get_or_init(|| HazardRegexes {
        payment: Regex::new(
            r"(?i)(?<!\p{L})(?:оплат(?:ить|а|еж|у|ы)?|купить|оформить\s+заказ|подтвердить\s+заказ|списать|перевести|подписаться|pay(?:\s+now)?|buy(?:\s+now)?|purchase|place\s+order|confirm\s+order|complete\s+order|charge|transfer|send\s+money|subscribe)(?!\p{L})",
        )
        .expect("payment regex"),
        destructive: Regex::new(
            r"(?i)(?<!\p{L})(?:удалить|стереть|очистить\s+вс[её]|сбросить|деактивировать|отвязать|заблокировать|отменить\s+заказ|delete|remove\s+account|erase|wipe|factory\s+reset|reset\s+all|destroy|uninstall|deactivate|terminate|revoke|purge|close\s+account|cancel\s+subscription)(?!\p{L})",
        )
        .expect("destructive regex"),
    })
}

const PAYMENT_RESOURCE_SUFFIXES: &[&str] = &[
    "btn_pay",
    "pay_button",
    "btn_buy",
    "buy_now",
    "buy_button",
    "confirm_purchase",
    "place_order",
    "btn_purchase",
];

const DESTRUCTIVE_RESOURCE_SUFFIXES: &[&str] = &[
    "btn_delete",
    "delete_button",
    "delete_account",
    "confirm_delete",
    "action_delete",
    "btn_erase",
    "btn_wipe",
    "factory_reset",
];

pub struct HazardousActionClassifier;

impl HazardousActionClassifier {
    pub fn assess(
        action: Option<&AttributedUIAction>,
        action_signature: &str,
        live_label: Option<&str>,
        live_resource_id: Option<&str>,
    ) -> HazardousActionAssessment {
        let mut labels: Vec<&str> = Vec::new();
        if let Some(lbl) = live_label {
            if !lbl.is_empty() {
                labels.push(lbl);
            }
        }
        if let Some(act) = action {
            if let Some(ref s) = act.selector_text {
                if !s.is_empty() {
                    labels.push(s.as_str());
                }
            }
            if let Some(ref d) = act.selector_content_desc {
                if !d.is_empty() {
                    labels.push(d.as_str());
                }
            }
            if !act.target_description.is_empty() {
                labels.push(act.target_description.as_str());
            }
        }
        let cleaned_sig = Self::strip_signature_prefix(action_signature);
        if !cleaned_sig.is_empty() {
            labels.push(cleaned_sig);
        }

        let mut resource_ids: Vec<&str> = Vec::new();
        if let Some(rid) = live_resource_id {
            if !rid.is_empty() {
                resource_ids.push(rid);
            }
        }
        if let Some(rid) = action.and_then(|a| a.selector_resource_id.as_deref()) {
            if !rid.is_empty() {
                resource_ids.push(rid);
            }
        }

        Self::evaluate(&labels, &resource_ids)
    }

    pub fn assess_node(node: &ShadowUINode) -> HazardousActionAssessment {
        let mut labels: Vec<&str> = Vec::new();
        if !node.text.is_empty() {
            labels.push(node.text.as_str());
        }
        if !node.content_desc.is_empty() {
            labels.push(node.content_desc.as_str());
        }
        let mut resource_ids: Vec<&str> = Vec::new();
        if !node.resource_id.is_empty() {
            resource_ids.push(node.resource_id.as_str());
        }
        Self::evaluate(&labels, &resource_ids)
    }

    fn evaluate(labels: &[&str], resource_ids: &[&str]) -> HazardousActionAssessment {
        let re = hazard_regexes();
        for &raw in labels {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(Some(m)) = re.payment.find(trimmed) {
                return HazardousActionAssessment {
                    is_hazardous: true,
                    category: Some(HazardCategory::Payment),
                    matched_pattern: Some(m.as_str().to_string()),
                    matched_label: Some(trimmed.to_string()),
                };
            }
            if let Ok(Some(m)) = re.destructive.find(trimmed) {
                return HazardousActionAssessment {
                    is_hazardous: true,
                    category: Some(HazardCategory::Destructive),
                    matched_pattern: Some(m.as_str().to_string()),
                    matched_label: Some(trimmed.to_string()),
                };
            }
        }

        for &raw_id in resource_ids {
            let short_id = ShadowTreeIndex::extract_short_resource_id(raw_id).to_lowercase();
            if short_id.is_empty() {
                continue;
            }
            if PAYMENT_RESOURCE_SUFFIXES.contains(&short_id.as_str())
                || short_id.ends_with("_pay")
                || short_id.ends_with("_checkout")
            {
                return HazardousActionAssessment {
                    is_hazardous: true,
                    category: Some(HazardCategory::Payment),
                    matched_pattern: Some(short_id.clone()),
                    matched_label: Some(
                        labels
                            .first()
                            .copied()
                            .unwrap_or(short_id.as_str())
                            .to_string(),
                    ),
                };
            }
            if DESTRUCTIVE_RESOURCE_SUFFIXES.contains(&short_id.as_str())
                || short_id.ends_with("_delete")
            {
                return HazardousActionAssessment {
                    is_hazardous: true,
                    category: Some(HazardCategory::Destructive),
                    matched_pattern: Some(short_id.clone()),
                    matched_label: Some(
                        labels
                            .first()
                            .copied()
                            .unwrap_or(short_id.as_str())
                            .to_string(),
                    ),
                };
            }
        }

        HazardousActionAssessment::safe()
    }

    fn strip_signature_prefix(signature: &str) -> &str {
        for prefix in ["tap:", "swipe:", "keyevent:", "typeText:", "launchApp:"] {
            if let Some(rest) = signature.strip_prefix(prefix) {
                return rest;
            }
        }
        signature
    }
}
