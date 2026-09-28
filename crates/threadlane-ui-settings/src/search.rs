/// A local, value-free settings destination exposed by the command palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettingsSearchItem {
    pub id: &'static str,
    pub title: &'static str,
    pub page: &'static str,
    pub keywords: &'static [&'static str],
}

/// Static metadata only: never populated from settings values or provider state.
pub const SETTINGS_SEARCH_ITEMS: &[SettingsSearchItem] = &[
    SettingsSearchItem {
        id: "general",
        title: "General",
        page: "General",
        keywords: &[
            "application",
            "updates",
            "upgrade",
            "auto review",
            "automatic review",
            "address pr reviews",
        ],
    },
    SettingsSearchItem {
        id: "appearance",
        title: "Appearance & Themes",
        page: "Appearance",
        keywords: &["theme", "dark", "light", "color"],
    },
    SettingsSearchItem {
        id: "keybindings",
        title: "Keybindings",
        page: "Keybindings",
        keywords: &["shortcuts", "keyboard", "hotkeys"],
    },
    SettingsSearchItem {
        id: "providers",
        title: "Models & Providers",
        page: "Providers",
        keywords: &[
            "github token",
            "github pat",
            "api key",
            "credentials",
            "provider",
            "model",
        ],
    },
    SettingsSearchItem {
        id: "fusion",
        title: "Fusion model",
        page: "Agent & Fusion",
        keywords: &["fusion", "reasoning", "delegation", "session mode"],
    },
    SettingsSearchItem {
        id: "subagents",
        title: "Agent & Fusion",
        page: "Agent & Fusion",
        keywords: &["agents", "subagents"],
    },
    SettingsSearchItem {
        id: "skills",
        title: "Skills Catalog",
        page: "Skills",
        keywords: &["skill", "capability"],
    },
    SettingsSearchItem {
        id: "extensions",
        title: "WASI Extensions",
        page: "WASI Extensions",
        keywords: &["extension", "wasm"],
    },
    SettingsSearchItem {
        id: "acp-agents",
        title: "ACP Agents",
        page: "ACP Agents",
        keywords: &["agent client protocol", "external agents"],
    },
];

#[cfg(test)]
mod tests {
    use super::SETTINGS_SEARCH_ITEMS;
    use std::collections::HashSet;

    #[test]
    fn settings_search_destinations_have_unique_ids_and_requested_aliases() {
        let ids = SETTINGS_SEARCH_ITEMS
            .iter()
            .map(|item| item.id)
            .collect::<HashSet<_>>();
        assert_eq!(ids.len(), SETTINGS_SEARCH_ITEMS.len());
        for term in [
            "theme",
            "upgrade",
            "auto review",
            "github token",
            "fusion",
            "shortcuts",
        ] {
            assert!(
                SETTINGS_SEARCH_ITEMS
                    .iter()
                    .any(|item| item.keywords.contains(&term)),
                "missing alias {term}"
            );
        }
    }
}
