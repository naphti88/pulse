// What the plugin saw end a turn: an API error, by its kind, in one file per
// session until the next turn starts. The answer before it still says what it
// said, so without this a turn that died looks like the last one that ended.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Failed {
    pub error: String,
    pub at: f64,
}

// by session id; a file with null was taken up again
pub fn failed_in(dir: &Path) -> HashMap<String, Failed> {
    let Ok(entries) = std::fs::read_dir(dir) else { return HashMap::new() };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let id = path.file_name()?.to_str()?.strip_suffix(".json")?.to_string();
            let failed: Option<Failed> = serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
            Some((id, failed?))
        })
        .collect()
}

// the words Claude Code classifies its API errors with
pub fn in_words(error: &str) -> &'static str {
    match error {
        "rate_limit" => "usage limit reached",
        "overloaded" => "the service was overloaded",
        "server_error" => "the service failed",
        "authentication_failed" | "oauth_org_not_allowed" | "verification_required" | "cloud_credential_error" => "sign-in failed",
        "billing_error" | "account_on_hold" => "a problem with the account",
        "invalid_request" => "the request was refused",
        "model_not_found" => "the model is not available",
        "max_output_tokens" => "the answer ran too long",
        _ => "an error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_says_what_ended_the_turn_and_null_nothing() {
        let dir = std::env::temp_dir().join(format!("pulse-failed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("a.json"), r#"{"error":"rate_limit","at":5}"#).expect("write");
        std::fs::write(dir.join("b.json"), "null").expect("write");
        std::fs::write(dir.join("c.json"), "not json").expect("write");
        let found = failed_in(&dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found["a"], Failed { error: "rate_limit".into(), at: 5.0 });
        std::fs::remove_dir_all(&dir).expect("clean up");
        assert!(failed_in(&dir).is_empty());
        assert_eq!(in_words("rate_limit"), "usage limit reached");
        assert_eq!(in_words("something new"), "an error");
    }
}
