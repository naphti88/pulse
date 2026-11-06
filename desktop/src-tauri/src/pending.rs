// What the plugin saw stand before a person: permission dialogs, questions of
// an MCP server and calls the engine sent to be asked for, a list in one file
// per session while they last. It is only a report:
// the transcript has the last word, a call that is over waits for nobody,
// whatever a file says (live.rs). Without the plugin there are no files, and
// the rule approval_guess goes by what the transcript shows alone.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Pending {
    pub tool: String,
    pub at: f64,
    // a subagent asked: its calls are not in the session's transcript
    #[serde(default)]
    pub agent: Option<String>,
    // a question of an MCP server, not a tool of its own: any MCP call it came in
    #[serde(default)]
    pub asks: bool,
    // the engine only said the call must be asked for: in auto mode a classifier
    // may let it through by itself, and no event says it did
    #[serde(default)]
    pub checked: bool,
}

// by session id; an empty list was answered
pub fn pending_in(dir: &Path) -> HashMap<String, Vec<Pending>> {
    let Ok(entries) = std::fs::read_dir(dir) else { return HashMap::new() };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let id = path.file_name()?.to_str()?.strip_suffix(".json")?.to_string();
            let text = std::fs::read_to_string(&path).ok()?;
            let list: Vec<Pending> = serde_json::from_str(&text).ok()?;
            (!list.is_empty()).then_some((id, list))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_says_what_waits_and_an_empty_list_nothing() {
        let dir = std::env::temp_dir().join(format!("pulse-pending-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("a.json"), r#"[{"tool":"Bash","at":5}]"#).expect("write");
        std::fs::write(dir.join("b.json"), "[]").expect("write");
        std::fs::write(dir.join("c.json"), r#"[{"tool":"Bash","at":4},{"tool":"mcp__","at":6,"asks":true,"agent":"x"}]"#).expect("write");
        let found = pending_in(&dir);
        assert_eq!(found.len(), 2);
        assert_eq!(found["a"], vec![Pending { tool: "Bash".into(), at: 5.0, agent: None, asks: false, checked: false }]);
        assert!(found["c"][1].asks);
        std::fs::remove_dir_all(&dir).expect("clean up");
        assert!(pending_in(&dir).is_empty());
    }
}
