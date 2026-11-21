use std::collections::{BTreeMap, BTreeSet, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::hasher::{ScreenLayoutHasher, ScreenSemanticFingerprint};
use super::models::{
    AttributedUIAction, ScreenStateVertex, ScreenTransitionEdge, SpatialGraphSnapshot,
    UIActionKind,
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedMergeReport {
    pub vertices_added: usize,
    pub edges_added: usize,
    pub aliases_merged: usize,
}

/// Cross-device, Zero-PII canonical Seed Graph catalog for Android Settings (`com.android.settings`).
///
/// All hashes, deterministic edge UUIDs, tokens, labels, and multilingual/OEM semantic aliases
/// are bit-exact identical to Swift `AndroidShadowGraphKit.SeedGraphCatalog`.
pub struct SeedGraphCatalog;

impl SeedGraphCatalog {
    pub const ANDROID_SETTINGS_PACKAGE: &'static str = "com.android.settings";
    pub const SEED_MATCH_THRESHOLD: f64 = 0.60;

    /// Fixed epoch (`2026-01-01T00:00:00Z`) so seed snapshots carry zero device/user timestamps.
    fn seed_epoch() -> DateTime<Utc> {
        DateTime::from_timestamp(1_767_225_600, 0).expect("valid seed epoch")
    }

    fn build_seed_vertex(
        title: &str,
        anchor_labels: &[&str],
        aliases: &[&str],
        parent_hash: Option<u64>,
        depth: i32,
    ) -> ScreenStateVertex {
        let mut tokens = BTreeSet::new();
        tokens.insert(format!(
            "TextView||{}",
            ScreenLayoutHasher::normalize_label(title)
        ));
        let mut sample_labels = Vec::with_capacity(anchor_labels.len() + 1);
        sample_labels.push(title.to_string());
        for &anchor in anchor_labels {
            let norm = ScreenLayoutHasher::normalize_label(anchor);
            tokens.insert(format!("TextView|android:id/title|{norm}"));
            if !sample_labels.iter().any(|existing| existing == anchor) {
                sample_labels.push(anchor.to_string());
            }
        }
        let hash = ScreenLayoutHasher::fnv1a64(Self::ANDROID_SETTINGS_PACKAGE, &tokens);
        let epoch = Self::seed_epoch();

        let mut semantic_aliases: Vec<String> = Vec::with_capacity(aliases.len() + 1);
        semantic_aliases.push(title.to_string());
        for &a in aliases {
            if !semantic_aliases
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(a))
            {
                semantic_aliases.push(a.to_string());
            }
        }

        ScreenStateVertex {
            layout_hash: hash,
            active_package: Self::ANDROID_SETTINGS_PACKAGE.to_string(),
            sample_interactive_labels: sample_labels,
            semantic_tokens: tokens,
            first_seen_at: epoch,
            last_seen_at: epoch,
            visit_count: 0,
            discovery_parent_hash: parent_hash,
            discovery_depth: depth,
            is_seed: true,
            semantic_aliases,
        }
    }

    fn build_seed_edge(
        uuid_str: &str,
        from_hash: u64,
        to_hash: u64,
        primary_selector: &str,
        selector_aliases: &[&str],
    ) -> ScreenTransitionEdge {
        let epoch = Self::seed_epoch();
        let action = AttributedUIAction {
            kind: UIActionKind::Tap,
            target_description: primary_selector.to_string(),
            x: None,
            y: None,
            end_x: None,
            end_y: None,
            duration_ms: None,
            text_input: None,
            press_enter: None,
            selector_resource_id: None,
            selector_text: Some(primary_selector.to_string()),
            selector_content_desc: None,
            selector_aliases: selector_aliases.iter().map(|s| (*s).to_string()).collect(),
            timestamp: epoch,
        };
        let action_signature = action.signature();
        ScreenTransitionEdge {
            id: Uuid::parse_str(uuid_str).expect("valid seed edge uuid"),
            from_hash,
            to_hash,
            action: Some(action),
            action_signature,
            traversal_count: 0,
            average_transition_ms: 250.0,
            is_spanning_tree_edge: true,
            last_traversed_at: epoch,
            is_hazardous: false,
            hazard_reason: None,
            consecutive_failures: 0,
            is_seed: true,
        }
    }

    /// Returns the deterministic, coordinate-free, Zero-PII Android Settings (`com.android.settings`)
    /// Seed Graph covering common AOSP / Pixel / OneUI / HyperOS / OxygenOS screens in EN & RU.
    pub fn android_settings_seed_graph() -> SpatialGraphSnapshot {
        let root = Self::build_seed_vertex(
            "Settings",
            &[
                "Search settings",
                "Network & internet",
                "Connected devices",
                "Apps",
                "Notifications",
                "Battery",
                "Storage",
                "Sound & vibration",
                "Display & touch",
                "Security & privacy",
                "Location",
                "Accessibility",
                "System",
                "About phone",
            ],
            &["Android Settings", "Настройки", "com.android.settings"],
            None,
            0,
        );
        let root_hash = root.layout_hash;

        let network = Self::build_seed_vertex(
            "Network & internet",
            &[
                "Internet",
                "SIMs",
                "Airplane mode",
                "Hotspot & tethering",
                "Data Saver",
                "VPN",
                "Private DNS",
            ],
            &[
                "Network and internet",
                "Connections",
                "Wi-Fi & network",
                "Сеть и интернет",
                "Подключения",
            ],
            Some(root_hash),
            1,
        );
        let internet = Self::build_seed_vertex(
            "Internet",
            &["Wi-Fi", "Add network", "Network preferences", "Saved networks"],
            &["Wi-Fi", "WLAN", "Интернет", "Wi‑Fi"],
            Some(network.layout_hash),
            2,
        );

        let connected_devices = Self::build_seed_vertex(
            "Connected devices",
            &[
                "Pair new device",
                "Connection preferences",
                "Bluetooth",
                "Cast",
                "NFC",
            ],
            &[
                "Bluetooth",
                "Bluetooth & devices",
                "Подключённые устройства",
                "Подключенные устройства",
            ],
            Some(root_hash),
            1,
        );

        let apps = Self::build_seed_vertex(
            "Apps",
            &[
                "See all apps",
                "Default apps",
                "Screen time",
                "Unused apps",
                "Special app access",
            ],
            &["Apps & notifications", "Applications", "Приложения"],
            Some(root_hash),
            1,
        );

        let notifications = Self::build_seed_vertex(
            "Notifications",
            &[
                "App notifications",
                "Notification history",
                "Conversations",
                "Bubbles",
                "Do Not Disturb",
            ],
            &["Notifications & status bar", "Уведомления"],
            Some(root_hash),
            1,
        );

        let battery = Self::build_seed_vertex(
            "Battery",
            &[
                "Battery usage",
                "Battery Saver",
                "Charging optimization",
                "Battery percentage",
                "Adaptive Battery",
            ],
            &["Battery & device care", "Power", "Батарея", "Питание"],
            Some(root_hash),
            1,
        );

        let storage = Self::build_seed_vertex(
            "Storage",
            &[
                "Free up space",
                "Images",
                "Videos",
                "Audio",
                "Documents & other",
                "Games",
                "Trash",
            ],
            &["Memory & storage", "Хранилище", "Память"],
            Some(root_hash),
            1,
        );

        let sound = Self::build_seed_vertex(
            "Sound & vibration",
            &[
                "Media volume",
                "Call volume",
                "Ring volume",
                "Alarm volume",
                "Do Not Disturb",
                "Phone ringtone",
                "Vibration & haptics",
            ],
            &[
                "Sound and vibration",
                "Sound",
                "Sounds and vibration",
                "Звук и вибрация",
                "Звук",
            ],
            Some(root_hash),
            1,
        );

        let display = Self::build_seed_vertex(
            "Display & touch",
            &[
                "Brightness level",
                "Adaptive brightness",
                "Lock screen",
                "Screen timeout",
                "Dark theme",
                "Night Light",
                "Display size and text",
                "Auto-rotate screen",
            ],
            &[
                "Display and touch",
                "Display",
                "Display & brightness",
                "Screen",
                "Экран",
                "Дисплей",
            ],
            Some(root_hash),
            1,
        );
        let night_light = Self::build_seed_vertex(
            "Night Light",
            &["Use Night Light", "Schedule", "Intensity"],
            &[
                "Night light",
                "Eye comfort shield",
                "Reading mode",
                "Ночная подсветка",
                "Ночной режим",
                "Защита зрения",
            ],
            Some(display.layout_hash),
            2,
        );
        let dark_theme = Self::build_seed_vertex(
            "Dark theme",
            &["Use Dark theme", "Schedule"],
            &["Dark mode", "Тёмная тема", "Темная тема"],
            Some(display.layout_hash),
            2,
        );

        let security = Self::build_seed_vertex(
            "Security & privacy",
            &[
                "Device unlock",
                "App security",
                "System & updates",
                "Privacy controls",
                "More security & privacy",
            ],
            &[
                "Security and privacy",
                "Security",
                "Privacy",
                "Biometrics and security",
                "Безопасность и конфиденциальность",
                "Безопасность",
            ],
            Some(root_hash),
            1,
        );

        let location = Self::build_seed_vertex(
            "Location",
            &[
                "Use location",
                "App location permissions",
                "Location services",
            ],
            &["Местоположение", "Геолокация"],
            Some(root_hash),
            1,
        );

        let accessibility = Self::build_seed_vertex(
            "Accessibility",
            &[
                "TalkBack",
                "Color and motion",
                "Magnification",
                "Caption preferences",
                "Timing controls",
            ],
            &["Специальные возможности"],
            Some(root_hash),
            1,
        );

        let system = Self::build_seed_vertex(
            "System",
            &[
                "Languages",
                "Keyboard",
                "Gestures",
                "Date & time",
                "Backup",
                "System update",
                "Developer options",
            ],
            &[
                "General management",
                "Additional settings",
                "Система",
                "Расширенные настройки",
            ],
            Some(root_hash),
            1,
        );
        let languages = Self::build_seed_vertex(
            "Languages",
            &["System languages", "App languages", "Regional preferences"],
            &[
                "Languages & input",
                "Language & region",
                "Language",
                "Языки",
                "Язык и ввод",
            ],
            Some(system.layout_hash),
            2,
        );
        let date_time = Self::build_seed_vertex(
            "Date & time",
            &[
                "Set time automatically",
                "Time zone",
                "Use 24-hour format",
            ],
            &["Date and time", "Дата и время"],
            Some(system.layout_hash),
            2,
        );
        let developer_options = Self::build_seed_vertex(
            "Developer options",
            &[
                "Use developer options",
                "Memory",
                "Stay awake",
                "USB debugging",
                "Wireless debugging",
                "Show layout bounds",
                "Window animation scale",
                "Animator duration scale",
            ],
            &["Developer mode", "Для разработчиков"],
            Some(system.layout_hash),
            2,
        );

        let about_phone = Self::build_seed_vertex(
            "About phone",
            &[
                "Device name",
                "Android version",
                "Model",
                "Build number",
                "IP address",
            ],
            &["About device", "About tablet", "О телефоне", "Об устройстве"],
            Some(root_hash),
            1,
        );

        let edges = vec![
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000001",
                root_hash,
                network.layout_hash,
                "Network & internet",
                &[
                    "Network and internet",
                    "Connections",
                    "Wi-Fi & network",
                    "Сеть и интернет",
                    "Подключения",
                ],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000002",
                network.layout_hash,
                internet.layout_hash,
                "Internet",
                &["Wi-Fi", "WLAN", "Интернет", "Wi‑Fi"],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000003",
                root_hash,
                connected_devices.layout_hash,
                "Connected devices",
                &[
                    "Bluetooth",
                    "Bluetooth & devices",
                    "Подключённые устройства",
                    "Подключенные устройства",
                ],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000004",
                root_hash,
                apps.layout_hash,
                "Apps",
                &["Apps & notifications", "Applications", "Приложения"],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000005",
                root_hash,
                notifications.layout_hash,
                "Notifications",
                &["Notifications & status bar", "Уведомления"],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000006",
                root_hash,
                battery.layout_hash,
                "Battery",
                &["Battery & device care", "Power", "Батарея", "Питание"],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000007",
                root_hash,
                storage.layout_hash,
                "Storage",
                &["Memory & storage", "Хранилище", "Память"],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000008",
                root_hash,
                sound.layout_hash,
                "Sound & vibration",
                &[
                    "Sound and vibration",
                    "Sound",
                    "Sounds and vibration",
                    "Звук и вибрация",
                    "Звук",
                ],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000009",
                root_hash,
                display.layout_hash,
                "Display & touch",
                &[
                    "Display and touch",
                    "Display",
                    "Display & brightness",
                    "Screen",
                    "Экран",
                    "Дисплей",
                ],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000010",
                display.layout_hash,
                night_light.layout_hash,
                "Night Light",
                &[
                    "Night light",
                    "Eye comfort shield",
                    "Reading mode",
                    "Ночная подсветка",
                    "Ночной режим",
                    "Защита зрения",
                ],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000011",
                display.layout_hash,
                dark_theme.layout_hash,
                "Dark theme",
                &["Dark mode", "Тёмная тема", "Темная тема"],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000012",
                root_hash,
                security.layout_hash,
                "Security & privacy",
                &[
                    "Security and privacy",
                    "Security",
                    "Privacy",
                    "Biometrics and security",
                    "Безопасность и конфиденциальность",
                    "Безопасность",
                ],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000013",
                root_hash,
                location.layout_hash,
                "Location",
                &["Местоположение", "Геолокация"],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000014",
                root_hash,
                accessibility.layout_hash,
                "Accessibility",
                &["Специальные возможности"],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000015",
                root_hash,
                system.layout_hash,
                "System",
                &[
                    "General management",
                    "Additional settings",
                    "Система",
                    "Расширенные настройки",
                ],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000016",
                system.layout_hash,
                languages.layout_hash,
                "Languages",
                &[
                    "Languages & input",
                    "Language & region",
                    "Language",
                    "Языки",
                    "Язык и ввод",
                ],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000017",
                system.layout_hash,
                date_time.layout_hash,
                "Date & time",
                &["Date and time", "Дата и время"],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000018",
                system.layout_hash,
                developer_options.layout_hash,
                "Developer options",
                &["Developer mode", "Для разработчиков"],
            ),
            Self::build_seed_edge(
                "A0000001-0000-4000-8000-000000000019",
                root_hash,
                about_phone.layout_hash,
                "About phone",
                &["About device", "About tablet", "О телефоне", "Об устройстве"],
            ),
        ];

        let mut spanning_children: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for edge in &edges {
            spanning_children
                .entry(edge.from_hash)
                .or_default()
                .push(edge.to_hash);
        }

        let vertices = vec![
            root,
            network,
            internet,
            connected_devices,
            apps,
            notifications,
            battery,
            storage,
            sound,
            display,
            night_light,
            dark_theme,
            security,
            location,
            accessibility,
            system,
            languages,
            date_time,
            developer_options,
            about_phone,
        ];

        SpatialGraphSnapshot {
            root_state_hash: Some(root_hash),
            current_state_hash: None,
            vertices,
            edges,
            spanning_children,
        }
    }

    /// Normalizes a UI label into a cross-OEM, cross-locale canonical concept key
    /// so Pixel, AOSP, Samsung OneUI, Xiaomi HyperOS, and EN/RU Android Settings match deterministically.
    pub fn canonical_concept_key(raw: &str) -> String {
        let trimmed = raw.trim().to_lowercase();
        if trimmed.is_empty() {
            return String::new();
        }
        let cleaned = trimmed
            .replace(['\u{2011}', '\u{2013}', '\u{2014}'], "-")
            .replace('&', " and ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        match cleaned.as_str() {
            // Root Settings & search
            "settings" | "android settings" | "настройки" => "settings".to_string(),
            "search settings" | "поиск настроек" | "поиск в настройках" => {
                "search_settings".to_string()
            }
            // Top-level categories
            "network and internet"
            | "connections"
            | "wi-fi and network"
            | "сеть и интернет"
            | "подключения" => "network_internet".to_string(),
            "internet" | "wi-fi" | "wifi" | "wlan" | "интернет" => "internet_wifi".to_string(),
            "connected devices"
            | "bluetooth"
            | "bluetooth and devices"
            | "подключённые устройства"
            | "подключенные устройства" => "connected_devices".to_string(),
            "apps" | "apps and notifications" | "applications" | "app management"
            | "приложения" => "apps".to_string(),
            "notifications" | "notifications and status bar" | "уведомления" => {
                "notifications".to_string()
            }
            "battery" | "battery and device care" | "power" | "батарея" | "питание" => {
                "battery".to_string()
            }
            "storage" | "memory and storage" | "хранилище" | "память" => "storage".to_string(),
            "sound and vibration"
            | "sound"
            | "sounds and vibration"
            | "звук и вибрация"
            | "звук" => "sound_vibration".to_string(),
            "display and touch"
            | "display"
            | "display and brightness"
            | "screen"
            | "экран"
            | "дисплей" => "display_touch".to_string(),
            "security and privacy"
            | "security"
            | "privacy"
            | "biometrics and security"
            | "безопасность и конфиденциальность"
            | "безопасность" => "security_privacy".to_string(),
            "location" | "местоположение" | "геолокация" => "location".to_string(),
            "accessibility" | "специальные возможности" => "accessibility".to_string(),
            "system" | "general management" | "additional settings" | "система"
            | "расширенные настройки" => "system".to_string(),
            "about phone" | "about device" | "about tablet" | "о телефоне" | "об устройстве" => {
                "about_phone".to_string()
            }
            // Display sub-items
            "brightness level" | "brightness" | "яркость" | "уровень яркости" => {
                "brightness_level".to_string()
            }
            "adaptive brightness" | "автояркость" | "адаптивная яркость" => {
                "adaptive_brightness".to_string()
            }
            "lock screen" | "экран блокировки" => "lock_screen".to_string(),
            "screen timeout" | "тайм-аут экрана" | "спящий режим" => {
                "screen_timeout".to_string()
            }
            "dark theme" | "dark mode" | "тёмная тема" | "темная тема" => "dark_theme".to_string(),
            "use dark theme" | "использовать тёмную тему" | "использовать темную тему" => {
                "use_dark_theme".to_string()
            }
            "night light"
            | "eye comfort shield"
            | "reading mode"
            | "ночная подсветка"
            | "ночной режим"
            | "защита зрения" => "night_light".to_string(),
            "use night light" | "включить ночную подсветку" => {
                "use_night_light".to_string()
            }
            "schedule" | "расписание" => "schedule".to_string(),
            "intensity" | "интенсивность" => "intensity".to_string(),
            "auto-rotate screen" | "автоповорот экрана" => "auto_rotate".to_string(),
            // System & Developer sub-items
            "languages"
            | "languages and input"
            | "language and region"
            | "language"
            | "языки"
            | "язык и ввод" => "languages".to_string(),
            "system languages" | "языки системы" => "system_languages".to_string(),
            "app languages" | "языки приложений" => "app_languages".to_string(),
            "date and time" | "дата и время" => "date_time".to_string(),
            "set time automatically" | "использовать время сети" | "время сети" => {
                "auto_time".to_string()
            }
            "time zone" | "часовой пояс" => "time_zone".to_string(),
            "developer options" | "developer mode" | "для разработчиков" => {
                "developer_options".to_string()
            }
            "use developer options" | "использовать параметры разработчика" => {
                "use_developer_options".to_string()
            }
            "usb debugging" | "отладка по usb" => "usb_debugging".to_string(),
            "wireless debugging" | "беспроводная отладка" => "wireless_debugging".to_string(),
            "show layout bounds" | "показывать границы элементов" | "границы элементов" => {
                "show_layout_bounds".to_string()
            }
            "stay awake" | "не выключать экран" => "stay_awake".to_string(),
            // Network / Battery / About sub-items
            "airplane mode" | "режим полёта" | "режим полета" => "airplane_mode".to_string(),
            "hotspot and tethering" | "точка доступа и модем" => "hotspot_tethering".to_string(),
            "private dns" | "персональный dns-сервер" => "private_dns".to_string(),
            "battery usage" | "использование батареи" => "battery_usage".to_string(),
            "battery saver" | "экономия заряда" => "battery_saver".to_string(),
            "battery percentage" | "заряд батареи в процентах" => {
                "battery_percentage".to_string()
            }
            "android version" | "версия android" => "android_version".to_string(),
            "build number" | "номер сборки" => "build_number".to_string(),
            "device name" | "имя устройства" => "device_name".to_string(),
            "pair new device" | "подключить новое устройство" => {
                "pair_new_device".to_string()
            }
            "app notifications" | "уведомления приложений" => "app_notifications".to_string(),
            "do not disturb" | "не беспокоить" => "do_not_disturb".to_string(),
            "see all apps" | "показать все приложения" => "see_all_apps".to_string(),
            "default apps" | "приложения по умолчанию" => "default_apps".to_string(),
            "use location" | "использовать геолокацию" => "use_location".to_string(),
            _ => cleaned,
        }
    }

    fn extract_live_concept_keys(
        labels: &[String],
        tokens: &BTreeSet<String>,
    ) -> HashSet<String> {
        let mut keys = HashSet::new();
        for raw in labels {
            let trimmed = raw.trim();
            if trimmed.is_empty() || (trimmed.starts_with('<') && trimmed.ends_with('>')) {
                continue;
            }
            let key = Self::canonical_concept_key(trimmed);
            if !key.is_empty() {
                keys.insert(key);
            }
        }
        for token in tokens {
            if let Some(tok_label) = token.splitn(3, '|').nth(2) {
                let trimmed = tok_label.trim();
                if trimmed.is_empty() || (trimmed.starts_with('<') && trimmed.ends_with('>')) {
                    continue;
                }
                let key = Self::canonical_concept_key(trimmed);
                if !key.is_empty() {
                    keys.insert(key);
                }
            }
        }
        keys
    }

    /// Computes a cross-device semantic match score in `[0.0, 1.0]` between a live screen fingerprint
    /// and an unhydrated Seed Graph vertex (`seed_vertex.is_seed == true`).
    ///
    /// Strictly enforces package equality and prevents parent/child false positives:
    /// - Hub/root seed (`discovery_depth == 0`) requires >= 3 matching anchor concepts.
    /// - Sub-screen seed (`discovery_depth > 0`) requires either (title/alias match AND >= 1 non-title
    ///   characteristic anchor) OR (>= 2 non-title characteristic anchors), so listing `"Display"` as a
    ///   single row on Settings Root never falsely matches the `Display` sub-screen.
    pub fn seed_match_score(
        live_fp: &ScreenSemanticFingerprint,
        seed_vertex: &ScreenStateVertex,
    ) -> f64 {
        if !seed_vertex.is_seed || live_fp.active_package != seed_vertex.active_package {
            return 0.0;
        }
        let live_keys =
            Self::extract_live_concept_keys(&live_fp.sample_interactive_labels, &live_fp.tokens);
        Self::seed_match_score_from_keys(&live_keys, seed_vertex)
    }

    pub fn seed_match_score_for_vertex(
        real_vertex: &ScreenStateVertex,
        seed_vertex: &ScreenStateVertex,
    ) -> f64 {
        if !seed_vertex.is_seed
            || real_vertex.is_seed
            || real_vertex.active_package != seed_vertex.active_package
        {
            return 0.0;
        }
        let live_keys = Self::extract_live_concept_keys(
            &real_vertex.sample_interactive_labels,
            &real_vertex.semantic_tokens,
        );
        Self::seed_match_score_from_keys(&live_keys, seed_vertex)
    }

    fn seed_match_score_from_keys(
        live_keys: &HashSet<String>,
        seed_vertex: &ScreenStateVertex,
    ) -> f64 {
        if live_keys.is_empty() {
            return 0.0;
        }

        let mut title_keys: HashSet<String> = HashSet::new();
        for alias in &seed_vertex.semantic_aliases {
            let k = Self::canonical_concept_key(alias);
            if !k.is_empty() {
                title_keys.insert(k);
            }
        }
        if let Some(first) = seed_vertex.sample_interactive_labels.first() {
            let k = Self::canonical_concept_key(first);
            if !k.is_empty() {
                title_keys.insert(k);
            }
        }

        let all_seed_anchors = Self::extract_live_concept_keys(
            &seed_vertex.sample_interactive_labels,
            &seed_vertex.semantic_tokens,
        );

        let title_matched = title_keys.iter().any(|k| live_keys.contains(k));
        let non_title_anchors: Vec<&String> = all_seed_anchors
            .iter()
            .filter(|k| !title_keys.contains(*k))
            .collect();
        let matched_non_title = non_title_anchors
            .iter()
            .filter(|k| live_keys.contains(**k))
            .count();

        if seed_vertex.discovery_depth == 0 {
            let total_matched = all_seed_anchors
                .iter()
                .filter(|k| live_keys.contains(*k))
                .count();
            if total_matched < 3 {
                return 0.0;
            }
            let denom = all_seed_anchors.len().clamp(3, 6) as f64;
            let ratio = (total_matched as f64 / denom).min(1.0);
            return 0.65 + 0.30 * ratio;
        }

        if title_matched && matched_non_title >= 1 {
            let denom = non_title_anchors.len().clamp(1, 3) as f64;
            let ratio = (matched_non_title as f64 / denom).min(1.0);
            return 0.76 + 0.20 * ratio;
        }

        if matched_non_title >= 2 {
            let denom = non_title_anchors.len().clamp(2, 4) as f64;
            let ratio = (matched_non_title as f64 / denom).min(1.0);
            return 0.64 + 0.22 * ratio;
        }

        0.0
    }
}
