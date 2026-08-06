//! Server configuration, supplied by Zed from `lsp.ebcdic-lsp.settings`.

use serde::Deserialize;

use crate::tables::{Codepage, CODEPAGES};

/// User settings.
///
/// Upstream contributes no configuration at all; this exists solely because 22 entries in a
/// code-action menu is noisier than 22 entries in a searchable command palette. Defaulting to
/// every codepage keeps parity with upstream out of the box.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Codepage ids to offer, e.g. `["0037", "1047"]`. Empty or absent means all of them.
    pub codepages: Vec<String>,
}

impl Settings {
    /// Parse settings from an LSP payload, tolerating both the bare object and a nested
    /// `{"ebcdic": {...}}` section, since Zed's settings plumbing can deliver either shape.
    ///
    /// Unparseable input yields defaults rather than an error: a malformed setting should
    /// degrade to "offer everything", never take the server down.
    pub fn from_value(value: &serde_json::Value) -> Self {
        let scoped = value.get("ebcdic").unwrap_or(value);
        serde_json::from_value(scoped.clone()).unwrap_or_default()
    }

    /// The codepages to offer, in upstream's declaration order.
    ///
    /// Unknown ids are ignored. If filtering leaves nothing (every id was a typo), fall back
    /// to all codepages so the feature never silently disappears.
    pub fn enabled_codepages(&self) -> Vec<&'static Codepage> {
        if self.codepages.is_empty() {
            return CODEPAGES.iter().collect();
        }
        let selected: Vec<&'static Codepage> = CODEPAGES
            .iter()
            .filter(|codepage| {
                self.codepages
                    .iter()
                    .any(|requested| normalize(requested) == codepage.id)
            })
            .collect();
        if selected.is_empty() {
            CODEPAGES.iter().collect()
        } else {
            selected
        }
    }

    /// Ids requested by the user that match no known codepage, for a startup warning.
    pub fn unknown_codepages(&self) -> Vec<&str> {
        self.codepages
            .iter()
            .filter(|requested| {
                !CODEPAGES
                    .iter()
                    .any(|codepage| normalize(requested) == codepage.id)
            })
            .map(String::as_str)
            .collect()
    }
}

/// Accept `37`, `037`, `0037`, and `cp1047` as ids, since upstream's zero-padded four-digit
/// form is easy to mistype and a silently-ignored setting is hard to debug.
fn normalize(requested: &str) -> String {
    let trimmed = requested.trim().to_ascii_lowercase();
    let digits = trimmed
        .trim_start_matches("cp")
        .trim_start_matches("ibm-")
        .trim_start_matches("ibm");
    format!("{:0>4}", digits.trim_start_matches('0'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_offers_every_codepage() {
        assert_eq!(Settings::default().enabled_codepages().len(), 11);
    }

    #[test]
    fn empty_list_offers_every_codepage() {
        let settings = Settings::from_value(&json!({ "codepages": [] }));
        assert_eq!(settings.enabled_codepages().len(), 11);
    }

    #[test]
    fn filters_to_requested_codepages_in_upstream_order() {
        let settings = Settings::from_value(&json!({ "codepages": ["1047", "0037"] }));
        let ids: Vec<&str> = settings.enabled_codepages().iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            ["0037", "1047"],
            "output follows upstream order, not user order"
        );
    }

    #[test]
    fn accepts_common_spellings_of_a_codepage_id() {
        for spelling in ["0037", "037", "37", "cp037", "CP37", "ibm-037", " 1047 "] {
            let settings = Settings::from_value(&json!({ "codepages": [spelling] }));
            assert_eq!(
                settings.enabled_codepages().len(),
                1,
                "{spelling} should match exactly one codepage"
            );
        }
    }

    #[test]
    fn reads_nested_ebcdic_section() {
        let settings = Settings::from_value(&json!({ "ebcdic": { "codepages": ["0500"] } }));
        let ids: Vec<&str> = settings.enabled_codepages().iter().map(|c| c.id).collect();
        assert_eq!(ids, ["0500"]);
    }

    #[test]
    fn all_unknown_ids_fall_back_to_every_codepage() {
        let settings = Settings::from_value(&json!({ "codepages": ["9999", "nonsense"] }));
        assert_eq!(settings.enabled_codepages().len(), 11);
        assert_eq!(settings.unknown_codepages(), ["9999", "nonsense"]);
    }

    #[test]
    fn unknown_ids_alongside_valid_ones_are_ignored_not_fatal() {
        let settings = Settings::from_value(&json!({ "codepages": ["0037", "9999"] }));
        let ids: Vec<&str> = settings.enabled_codepages().iter().map(|c| c.id).collect();
        assert_eq!(ids, ["0037"]);
        assert_eq!(settings.unknown_codepages(), ["9999"]);
    }

    #[test]
    fn malformed_settings_degrade_to_defaults() {
        for value in [json!({ "codepages": "0037" }), json!("nonsense"), json!(42)] {
            assert_eq!(
                Settings::from_value(&value).enabled_codepages().len(),
                11,
                "malformed settings must not disable the feature"
            );
        }
    }
}
