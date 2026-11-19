use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;

use fancy_regex::Regex;
use serde::{Deserialize, Serialize};

use crate::protocol::ShadowUINode;
use crate::shadow_dom::ShadowTreeIndex;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenSemanticFingerprint {
    pub layout_hash: u64,
    pub active_package: String,
    pub active_window_id: i32,
    pub tokens: BTreeSet<String>,
    pub sample_interactive_labels: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TemporalKind {
    Clock,
    Date,
}

impl TemporalKind {
    fn token(self) -> &'static str {
        match self {
            Self::Clock => "<CLOCK>",
            Self::Date => "<DATE>",
        }
    }
}

struct HasherRegexes {
    time: Regex,
    weekday: Regex,
    month: Regex,
    march_may_ahead: Regex,
    march_may_behind: Regex,
    sun_ahead: Regex,
    sun_behind: Regex,
    relative_day: Regex,
    date_fragment: Regex,
    price_prefix: Regex,
    price_suffix: Regex,
    eta_time: Regex,
    eta: Regex,
    collapse_eta: Regex,
    weight_multi: Regex,
    weight_single: Regex,
    otp_after_marker: Regex,
    otp_before_marker: Regex,
}

fn regexes() -> &'static HasherRegexes {
    static RE: OnceLock<HasherRegexes> = OnceLock::new();
    RE.get_or_init(|| HasherRegexes {
        time: Regex::new(
            r"(?i)(?<!\p{L})#(?:\s*:\s*#){1,2}(?:\s*(?:[ap]\.?m\.?|д\.?п\.?|п\.?п\.?))?(?!\p{L})",
        )
        .expect("time regex"),
        weekday: Regex::new(
            r"(?i)(?<!\p{L})(?:monday|tuesday|wednesday|thursday|friday|saturday|sunday|mon\.?|tues?\.?|wed\.?|thur?s?\.?|fri\.?|sat\.?|понедельник|вторник|сред[ау]|четверг|пятниц[ау]|суббот[ау]|воскресенье|пн\.?|вт\.?|ср\.?|чт\.?|пт\.?|сб\.?|вс\.?)(?!\p{L})",
        )
        .expect("weekday regex"),
        month: Regex::new(
            r"(?i)(?<!\p{L})(?:january|jan\.?|february|feb\.?|april|apr\.?|june|jun\.?|july|jul\.?|august|aug\.?|september|sept?\.?|october|oct\.?|november|nov\.?|december|dec\.?|январ[ья]|янв\.?|феврал[ья]|февр?\.?|марта?|мар\.?|апрел[ья]|апр\.?|ма[йя]|июн(?:[ья]|\.)?|июл(?:[ья]|\.)?|августа?|авг\.?|сентябр[ья]|сент?\.?|октябр[ья]|окт\.?|ноябр[ья]|нояб?\.?|декабр[ья]|дек\.?)(?!\p{L})",
        )
        .expect("month regex"),
        // Case-sensitive March/Mar./May contextual match (lookahead + capture-prefix rewrite avoiding variable-length lookbehind)
        march_may_ahead: Regex::new(
            r"(?<!\p{L})(?:March|Mar\.?|May)(?=\s*,\s*#|\s+#)",
        )
        .expect("march_may_ahead regex"),
        march_may_behind: Regex::new(
            r"((?:#\.?|<DAY>)[\s,]{1,2})(?:March|Mar\.?|May)(?!\p{L})",
        )
        .expect("march_may_behind regex"),
        // Case-sensitive Sun/Sun. contextual match
        sun_ahead: Regex::new(
            r"(?<!\p{L})Sun\.?(?=\s*,\s*(?:#|<MONTH>|<TIME>)|\s+(?:#|<MONTH>|<TIME>))",
        )
        .expect("sun_ahead regex"),
        sun_behind: Regex::new(
            r"((?:#|<MONTH>|<TIME>)[,\s]{1,3})Sun\.?(?!\p{L})",
        )
        .expect("sun_behind regex"),
        relative_day: Regex::new(
            r"(?i)(?<!\p{L})(?:today|tomorrow|yesterday|сегодня|завтра|вчера)(?!\p{L})",
        )
        .expect("relative_day regex"),
        date_fragment: Regex::new(
            r"(?:#\.?\s+<MONTH>|<MONTH>[,\s]+#\.?)(?:\s*,?\s*#(?:\s*г\.?)?)?",
        )
        .expect("date_fragment regex"),
        price_prefix: Regex::new(r"[$€£¥₽₴₸₺₹]\s*#(?:[\s.,]#)*").expect("price_prefix regex"),
        price_suffix: Regex::new(
            r"(?i)#(?:[\s.,]#)*\s*(?:[$€£¥₽₴₸₺₹](?!\s*#)|(?<!\p{L})(?:руб\.?|usd|eur|gbp|rub|uah|kzt|byn|pln|chf|cad|aud)(?!\p{L})|(?<!\p{L})р\.(?!\p{L}))",
        )
        .expect("price_suffix regex"),
        eta_time: Regex::new(r"(?i)(?<!\p{L})eta[:\s]+<TIME>").expect("eta_time regex"),
        eta: Regex::new(
            r"(?i)(?<!\p{L})(?:(?:~|≈|через\s+|in\s+|eta[:\s]+)\s*)?#(?:\s*[-–—]\s*#)?\s*(?:min(?:s|utes?)?|hr(?:s)?|hours?|sec(?:s|onds?)?|days?|мин(?:ут[уы]?)?\.?|час(?:а|ов)?\.?|ч\.|сек\.?|дн(?:я|ей)?\.?)(?!\p{L})",
        )
        .expect("eta regex"),
        collapse_eta: Regex::new(r"<ETA>(?:\s+<ETA>)+").expect("collapse_eta regex"),
        weight_multi: Regex::new(
            r"(?i)#(?:[.,]#)?\s*(?:kg|mg|lbs?|oz|ml|кг|гр\.?|мг|мл)(?!\p{L})",
        )
        .expect("weight_multi regex"),
        // Case-sensitive for single-letter units so "5G" cellular indicators are never tokenized as weights.
        weight_single: Regex::new(r"#(?:[.,]#)?\s*(?:g|l|г\.?|л\.?)(?!\p{L})")
            .expect("weight_single regex"),
        otp_after_marker: Regex::new(
            r"(?i)((?<![\p{L}\d])(?:code|otp|pin|verification|2fa|код|подтверждени[ея]|смс|sms)(?![\p{L}\d])[^\r\n.!?;]{0,32}?(?:\r?\n[ \t]*)?)(?<!\d[./-])(?<!\d)(?:\d{4,8}|\d{3}[ -]\d{3})(?!\d)(?![./-]\d)",
        )
        .expect("otp_after_marker regex"),
        otp_before_marker: Regex::new(
            r"(?i)(?<!\d[./-])(?<!\d)(?:\d{4,8}|\d{3}[ -]\d{3})(?!\d)(?![./-]\d)((?:\r?\n[ \t]*)?[^\r\n.!?;]{0,32}?(?<![\p{L}\d])(?:code|otp|pin|verification|2fa|код|подтверждени[ея]|смс|sms)(?![\p{L}\d]))",
        )
        .expect("otp_before_marker regex"),
    })
}

const DECORATIVE_IMAGE_CLASSES: &[&str] = &[
    "ImageView",
    "AppCompatImageView",
    "ShapeableImageView",
    "DraweeView",
    "SimpleDraweeView",
    "Image",
];

const CLOCK_CLASS_SUFFIXES: &[&str] = &[
    "Clock",
    "TextClock",
    "AnalogClock",
    "DigitalClock",
    "Chronometer",
    "DateTimeView",
];

const CLOCK_RESOURCE_IDS: &[&str] = &[
    "clock",
    "a11y_clock",
    "digital_clock",
    "analog_clock",
    "analogue_clock",
    "analog_appwidget",
    "digital_appwidget",
    "clock_display",
    "status_bar_clock",
    "qs_clock",
    "keyguard_clock",
    "keyguard_clock_view",
    "keyguard_clock_container",
    "lockscreen_clock",
    "lockscreen_clock_view",
    "big_clock",
    "small_clock",
    "aod_clock",
    "world_clock",
    "city_clock",
    "widget_clock",
    "clock_time",
    "clock_view",
    "time_view",
    "hours",
    "minutes",
    "seconds",
    "am_pm",
    "next_alarm",
    "nextalarm",
];

const DATE_RESOURCE_IDS: &[&str] = &[
    "date",
    "date_text",
    "date_view",
    "date_display",
    "clock_date",
    "date_clock",
    "smartspace_date",
    "keyguard_date",
    "lockscreen_date",
    "weekday",
    "day_of_week",
];

const EMBEDDED_CLOCK_WIDGET_PACKAGES: &[&str] = &[
    "com.google.android.deskclock",
    "com.android.deskclock",
    "com.android.alarmclock",
    "com.sec.android.app.clockpackage",
    "com.oneplus.deskclock",
    "com.oplus.alarmclock",
    "com.coloros.alarmclock",
    "com.miui.clock",
];

/// Produces a deterministic structural-semantic fingerprint of an Android screen.
/// Bit-exact compatible with Swift `AndroidShadowGraphKit.ScreenLayoutHasher`.
#[derive(Debug, Clone)]
pub struct ScreenLayoutHasher {
    pub ignored_packages: HashSet<String>,
}

impl Default for ScreenLayoutHasher {
    fn default() -> Self {
        let mut ignored = HashSet::new();
        ignored.insert("com.android.systemui".to_string());
        Self {
            ignored_packages: ignored,
        }
    }
}

impl ScreenLayoutHasher {
    pub fn new(ignored_packages: HashSet<String>) -> Self {
        Self { ignored_packages }
    }

    pub fn fingerprint(&self, index: &ShadowTreeIndex) -> ScreenSemanticFingerprint {
        let effective_active_package = Self::resolve_effective_active_package(
            &index.active_package,
            &index.ordered_nodes,
            &self.ignored_packages,
        );
        let skip_ignored = !self.ignored_packages.contains(&effective_active_package);

        let mut token_set = BTreeSet::new();
        let mut non_temporal_labels: Vec<String> = Vec::new();
        let mut temporal_labels: Vec<String> = Vec::new();
        let mut seen_labels: HashSet<String> = HashSet::new();

        for node in &index.ordered_nodes {
            if !node.visible {
                continue;
            }
            if skip_ignored && Self::is_ignored_system_node(node, &self.ignored_packages) {
                continue;
            }
            if Self::is_decorative_banner_image(node) {
                continue;
            }

            let raw_label = if !node.text.is_empty() {
                &node.text
            } else {
                &node.content_desc
            };
            let temporal_kind = Self::temporal_kind_for_node(node, &effective_active_package);
            let normalized_label = Self::normalize_label(raw_label);
            let is_interactive = node.clickable || node.scrollable || node.editable;

            let has_identity =
                !node.resource_id.is_empty() || is_interactive || !normalized_label.is_empty();
            if !has_identity {
                continue;
            }

            let token_label: &str = if let Some(kind) = temporal_kind {
                if !normalized_label.is_empty() {
                    kind.token()
                } else {
                    ""
                }
            } else if !node.resource_id.is_empty()
                || is_interactive
                || normalized_label.chars().count() <= 32
            {
                &normalized_label
            } else {
                "<TEXT>"
            };

            let token = format!("{}|{}|{}", node.class_name, node.resource_id, token_label);
            token_set.insert(token);

            if !raw_label.is_empty() && (is_interactive || !node.resource_id.is_empty()) {
                let safe_label = Self::redact_pii(raw_label);
                let compact_label: String = safe_label.chars().take(40).collect();
                if seen_labels.insert(compact_label.clone()) {
                    if temporal_kind.is_some() {
                        if temporal_labels.len() < 8 {
                            temporal_labels.push(compact_label);
                        }
                    } else if non_temporal_labels.len() < 8 {
                        non_temporal_labels.push(compact_label);
                    }
                }
            }
        }

        let mut interactive_labels = non_temporal_labels;
        interactive_labels.extend(temporal_labels);
        interactive_labels.truncate(8);

        let hash = Self::fnv1a64(&effective_active_package, &token_set);

        ScreenSemanticFingerprint {
            layout_hash: hash,
            active_package: effective_active_package,
            active_window_id: index.active_window_id,
            tokens: token_set,
            sample_interactive_labels: interactive_labels,
        }
    }

    /// Overlay masking (#3): when `reported_package` is an overlay package (e.g. `com.android.systemui`)
    /// while foreground app nodes are visible beneath it, anchor to the dominant visible foreground package.
    fn resolve_effective_active_package(
        reported_package: &str,
        nodes: &[ShadowUINode],
        ignored_packages: &HashSet<String>,
    ) -> String {
        if !ignored_packages.contains(reported_package) {
            return reported_package.to_string();
        }
        let mut package_counts: HashMap<&str, usize> = HashMap::new();
        for node in nodes {
            if !node.visible {
                continue;
            }
            let pkg = node.package_name.as_str();
            if pkg.is_empty() || ignored_packages.contains(pkg) {
                continue;
            }
            let has_content = node.clickable
                || node.scrollable
                || node.editable
                || !node.resource_id.is_empty()
                || !node.text.is_empty()
                || !node.content_desc.is_empty();
            if !has_content {
                continue;
            }
            *package_counts.entry(pkg).or_insert(0) += 1;
        }
        package_counts
            .into_iter()
            .max_by(|(pkg_a, cnt_a), (pkg_b, cnt_b)| {
                cnt_a.cmp(cnt_b).then_with(|| pkg_b.cmp(pkg_a))
            })
            .map(|(pkg, _)| pkg.to_string())
            .unwrap_or_else(|| reported_package.to_string())
    }

    fn is_image_view_class_name(class_name: &str) -> bool {
        let short = Self::short_class_name(class_name);
        DECORATIVE_IMAGE_CLASSES.contains(&short) || short.ends_with("ImageView")
    }

    /// Visual noise reduction (#6): excludes non-interactive image views lacking an explicit `resource_id`.
    fn is_decorative_banner_image(node: &ShadowUINode) -> bool {
        if !node.resource_id.is_empty() || node.clickable || node.scrollable || node.editable {
            return false;
        }
        Self::is_image_view_class_name(&node.class_name)
    }

    pub fn is_unanchored_image_token(token: &str) -> bool {
        let mut parts = token.splitn(3, '|');
        let Some(cls) = parts.next() else {
            return false;
        };
        let Some(rid) = parts.next() else {
            return false;
        };
        let Some(_) = parts.next() else {
            return false;
        };
        rid.is_empty() && Self::is_image_view_class_name(cls)
    }

    #[inline]
    pub fn similarity(lhs: &ScreenSemanticFingerprint, rhs: &ScreenSemanticFingerprint) -> f64 {
        if lhs.active_package != rhs.active_package {
            return 0.0;
        }
        Self::jaccard_similarity(&lhs.tokens, &rhs.tokens)
    }

    #[inline]
    pub fn jaccard_similarity(lhs: &BTreeSet<String>, rhs: &BTreeSet<String>) -> f64 {
        if lhs.is_empty() && rhs.is_empty() {
            return 1.0;
        }
        if lhs.is_empty() || rhs.is_empty() {
            return 0.0;
        }
        let (smaller, larger) = if lhs.len() <= rhs.len() {
            (lhs, rhs)
        } else {
            (rhs, lhs)
        };
        let mut intersection = 0usize;
        for token in smaller {
            if larger.contains(token) {
                intersection += 1;
            }
        }
        let union = lhs.len() + rhs.len() - intersection;
        if union == 0 {
            1.0
        } else {
            intersection as f64 / union as f64
        }
    }

    #[inline]
    pub fn package_similarity(
        package_a: &str,
        tokens_a: &BTreeSet<String>,
        package_b: &str,
        tokens_b: &BTreeSet<String>,
    ) -> f64 {
        if package_a != package_b {
            return 0.0;
        }
        Self::jaccard_similarity(tokens_a, tokens_b)
    }

    fn is_ignored_system_node(node: &ShadowUINode, ignored_packages: &HashSet<String>) -> bool {
        if !node.package_name.is_empty() && ignored_packages.contains(&node.package_name) {
            return true;
        }
        if !node.resource_id.is_empty() {
            for pkg in ignored_packages {
                if node.resource_id.starts_with(pkg)
                    && node.resource_id.as_bytes().get(pkg.len()) == Some(&b':')
                {
                    return true;
                }
            }
        }
        false
    }

    fn short_class_name(class_name: &str) -> &str {
        class_name
            .rfind('.')
            .map(|idx| &class_name[idx + 1..])
            .unwrap_or(class_name)
    }

    fn short_resource_id(resource_id: &str) -> String {
        ShadowTreeIndex::extract_short_resource_id(resource_id).to_lowercase()
    }

    fn resource_package(resource_id: &str) -> Option<&str> {
        let idx = resource_id.find(':')?;
        if idx == 0 {
            return None;
        }
        Some(&resource_id[..idx])
    }

    fn temporal_kind_for_node(node: &ShadowUINode, active_package: &str) -> Option<TemporalKind> {
        Self::temporal_kind(
            &node.class_name,
            &node.resource_id,
            &node.package_name,
            active_package,
        )
    }

    fn temporal_kind(
        class_name: &str,
        resource_id: &str,
        package_name: &str,
        active_package: &str,
    ) -> Option<TemporalKind> {
        let short_class = Self::short_class_name(class_name);
        if CLOCK_CLASS_SUFFIXES.contains(&short_class) {
            return Some(TemporalKind::Clock);
        }

        if !resource_id.is_empty() {
            let short_id = Self::short_resource_id(resource_id);
            if CLOCK_RESOURCE_IDS.contains(&short_id.as_str())
                || short_id.ends_with("_clock")
                || short_id.ends_with("_a11y_clock")
            {
                return Some(TemporalKind::Clock);
            }
            if DATE_RESOURCE_IDS.contains(&short_id.as_str()) || short_id.ends_with("_date") {
                return Some(TemporalKind::Date);
            }
        }

        if !package_name.is_empty()
            && package_name != active_package
            && EMBEDDED_CLOCK_WIDGET_PACKAGES.contains(&package_name)
        {
            return Some(TemporalKind::Clock);
        }

        if let Some(res_pkg) = Self::resource_package(resource_id) {
            if res_pkg != active_package && EMBEDDED_CLOCK_WIDGET_PACKAGES.contains(&res_pkg) {
                return Some(TemporalKind::Clock);
            }
        }

        None
    }

    pub fn canonicalize_token(token: &str, active_package: &str) -> String {
        let mut parts = token.splitn(3, '|');
        let (Some(class_name), Some(resource_id), Some(label)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return token.to_string();
        };
        if label.is_empty()
            || matches!(
                label,
                "<TEXT>" | "<CLOCK>" | "<DATE>" | "<PRICE>" | "<ETA>" | "<WEIGHT>" | "<CARD>" | "<OTP>"
            )
        {
            return token.to_string();
        }
        if let Some(kind) = Self::temporal_kind(class_name, resource_id, "", active_package) {
            return format!("{class_name}|{resource_id}|{}", kind.token());
        }
        let normalized = Self::normalize_label(label);
        format!("{class_name}|{resource_id}|{normalized}")
    }

    fn passes_luhn(digits: &[u8]) -> bool {
        let mut sum: u32 = 0;
        let mut double = false;
        for &d in digits.iter().rev() {
            let mut val = u32::from(d);
            if double {
                val *= 2;
                if val > 9 {
                    val -= 9;
                }
            }
            sum += val;
            double = !double;
        }
        sum.is_multiple_of(10)
    }

    fn is_card_separator(c: char) -> bool {
        c == ' ' || c == '-' || c == '\u{00A0}'
    }

    fn is_date_or_time_continuation(chars: &[char], idx: usize) -> bool {
        idx + 1 < chars.len()
            && (chars[idx] == '/' || chars[idx] == ':')
            && chars[idx + 1].is_ascii_digit()
    }

    fn redact_card_numbers(input: &str) -> String {
        let chars: Vec<char> = input.chars().collect();
        let n = chars.len();
        let mut out = String::with_capacity(input.len());
        let mut i = 0usize;

        while i < n {
            let ch = chars[i];
            if ch.is_ascii_digit() && (i == 0 || !chars[i - 1].is_ascii_digit()) {
                let mut first_end = i;
                while first_end < n && chars[first_end].is_ascii_digit() {
                    first_end += 1;
                }
                let first_len = first_end - i;
                let preceded_by_date_time = i >= 2
                    && (chars[i - 1] == '/' || chars[i - 1] == ':')
                    && chars[i - 2].is_ascii_digit();

                if first_len > 19
                    || preceded_by_date_time
                    || Self::is_date_or_time_continuation(&chars, first_end)
                {
                    for &c in &chars[i..first_end] {
                        out.push(c);
                    }
                    i = first_end;
                    continue;
                }

                let mut digits: Vec<u8> = Vec::with_capacity(20);
                for &c in &chars[i..first_end] {
                    digits.push((c as u8) - b'0');
                }

                let mut best_end: Option<usize> = None;
                if (13..=19).contains(&digits.len())
                    && digits.iter().any(|&d| d != digits[0])
                    && Self::passes_luhn(&digits)
                {
                    best_end = Some(first_end);
                }

                if first_len >= 4 {
                    let mut cursor = first_end;
                    let mut sep_is_hyphen: Option<bool> = None;
                    while cursor < n && digits.len() < 19 {
                        let mut sep_end = cursor;
                        while sep_end < n
                            && sep_end - cursor < 2
                            && Self::is_card_separator(chars[sep_end])
                        {
                            sep_end += 1;
                        }
                        if sep_end == cursor || sep_end >= n || !chars[sep_end].is_ascii_digit() {
                            break;
                        }
                        let cur_has_hyphen = chars[cursor..sep_end].contains(&'-');
                        let cur_has_space = chars[cursor..sep_end]
                            .iter()
                            .any(|&c| c == ' ' || c == '\u{00A0}');
                        if cur_has_hyphen && cur_has_space {
                            break;
                        }
                        if let Some(expected_hyphen) = sep_is_hyphen {
                            if cur_has_hyphen != expected_hyphen {
                                break;
                            }
                        } else {
                            sep_is_hyphen = Some(cur_has_hyphen);
                        }

                        let mut group_end = sep_end;
                        while group_end < n && chars[group_end].is_ascii_digit() {
                            group_end += 1;
                        }
                        if Self::is_date_or_time_continuation(&chars, group_end) {
                            break;
                        }
                        let group_len = group_end - sep_end;
                        if digits.len() + group_len > 19 {
                            break;
                        }
                        for &c in &chars[sep_end..group_end] {
                            digits.push((c as u8) - b'0');
                        }
                        cursor = group_end;

                        if (13..=19).contains(&digits.len())
                            && digits.iter().any(|&d| d != digits[0])
                            && Self::passes_luhn(&digits)
                        {
                            best_end = Some(cursor);
                        }
                    }
                }

                if let Some(end) = best_end {
                    out.push_str("<CARD>");
                    i = end;
                } else {
                    for &c in &chars[i..first_end] {
                        out.push(c);
                    }
                    i = first_end;
                }
            } else {
                out.push(ch);
                i += 1;
            }
        }
        out
    }

    /// Scrubs Luhn-valid payment card PANs (`<CARD>`) and context-anchored OTP/2FA codes (`<OTP>`)
    /// while leaving ordinary numbers (`2026`, `1080`, timestamps, repeated zeros) untouched.
    /// Bit-exact compatible with Swift `ScreenLayoutHasher.redactPII(_:)`.
    pub fn redact_pii(raw: &str) -> String {
        if !raw.bytes().any(|b| b.is_ascii_digit()) {
            return raw.to_string();
        }
        let mut result = Self::redact_card_numbers(raw);
        if result.bytes().any(|b| b.is_ascii_digit()) {
            let re = regexes();
            result = re
                .otp_after_marker
                .replace_all(&result, "${1}<OTP>")
                .into_owned();
            result = re
                .otp_before_marker
                .replace_all(&result, "<OTP>${1}")
                .into_owned();
        }
        result
    }

    pub fn normalize_label(raw: &str) -> String {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return String::new();
        }
        let redacted = Self::redact_pii(trimmed);

        let mut collapsed = String::with_capacity(redacted.len().min(256));
        let mut scalar_count = 0usize;
        let mut in_digits = false;
        let mut last_was_space = false;

        for ch in redacted.chars() {
            if ch.is_ascii_digit() {
                if !in_digits {
                    collapsed.push('#');
                    scalar_count += 1;
                    in_digits = true;
                }
                last_was_space = false;
            } else if ch.is_whitespace() {
                in_digits = false;
                if !last_was_space {
                    collapsed.push(' ');
                    scalar_count += 1;
                    last_was_space = true;
                }
            } else {
                in_digits = false;
                last_was_space = false;
                collapsed.push(ch);
                scalar_count += 1;
            }
            if scalar_count >= 256 {
                break;
            }
        }

        let re = regexes();
        let mut normalized = collapsed.trim().to_string();
        normalized = re.time.replace_all(&normalized, "<TIME>").into_owned();

        let has_time_or_digit = normalized.contains('#') || normalized.contains("<TIME>");
        if has_time_or_digit {
            normalized = re.month.replace_all(&normalized, "<MONTH>").into_owned();
            normalized = re.weekday.replace_all(&normalized, "<DAY>").into_owned();
            // Run March/May before Sun so "Sun, May 15" becomes "Sun, <MONTH> #" before sun checks it.
            normalized = re
                .march_may_ahead
                .replace_all(&normalized, "<MONTH>")
                .into_owned();
            normalized = re
                .march_may_behind
                .replace_all(&normalized, "${1}<MONTH>")
                .into_owned();
            normalized = re.sun_ahead.replace_all(&normalized, "<DAY>").into_owned();
            normalized = re
                .sun_behind
                .replace_all(&normalized, "${1}<DAY>")
                .into_owned();
            normalized = re
                .date_fragment
                .replace_all(&normalized, "<DATE>")
                .into_owned();
        }
        if normalized.contains("<TIME>") {
            normalized = re
                .relative_day
                .replace_all(&normalized, "<DAY>")
                .into_owned();
            normalized = re.eta_time.replace_all(&normalized, "<ETA>").into_owned();
        }
        if normalized.contains('#') {
            normalized = re
                .price_prefix
                .replace_all(&normalized, "<PRICE>")
                .into_owned();
            normalized = re
                .price_suffix
                .replace_all(&normalized, "<PRICE>")
                .into_owned();
            normalized = re.eta.replace_all(&normalized, "<ETA>").into_owned();
            normalized = re
                .weight_multi
                .replace_all(&normalized, "<WEIGHT>")
                .into_owned();
            normalized = re
                .weight_single
                .replace_all(&normalized, "<WEIGHT>")
                .into_owned();
        }
        if normalized.contains("<ETA>") {
            normalized = re
                .collapse_eta
                .replace_all(&normalized, "<ETA>")
                .into_owned();
        }

        let trimmed_norm = normalized.trim();
        if trimmed_norm.chars().count() <= 48 {
            return trimmed_norm.to_string();
        }
        trimmed_norm
            .chars()
            .take(48)
            .collect::<String>()
            .trim()
            .to_string()
    }

    /// Process-independent deterministic FNV-1a 64-bit hash (bit-exact with Swift `ScreenLayoutHasher`).
    pub fn fnv1a64(active_package: &str, sorted_tokens: &BTreeSet<String>) -> u64 {
        let mut hash: u64 = 14_695_981_039_346_656_037;
        const PRIME: u64 = 1_099_511_628_211;

        #[inline(always)]
        fn mix_bytes(hash: &mut u64, s: &str) {
            for &byte in s.as_bytes() {
                *hash ^= u64::from(byte);
                *hash = hash.wrapping_mul(PRIME);
            }
            *hash ^= 0x1F;
            *hash = hash.wrapping_mul(PRIME);
        }

        mix_bytes(&mut hash, active_package);
        for token in sorted_tokens {
            mix_bytes(&mut hash, token);
        }
        hash
    }
}
