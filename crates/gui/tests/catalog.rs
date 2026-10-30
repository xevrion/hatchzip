//! Detect catalog entries whose English source message has been removed.
use std::collections::HashSet;
use std::path::Path;

fn quoted(text: &str) -> String {
    // Both PO and the messages here use quoted strings with escaped quotes.
    let mut escaped = false;
    for (i, c) in text.char_indices().skip(1) {
        if c == '"' && !escaped {
            return serde_json::from_str(&text[..=i]).expect("valid message literal");
        }
        escaped = c == '\\' && !escaped;
    }
    panic!("unterminated message: {text}");
}

fn catalog_ids(po: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = None;
    for line in po.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("msgid ") {
            if let Some(id) = current.take() {
                result.push(id);
            }
            current = Some(quoted(rest));
        } else if line.starts_with('"') {
            if let Some(id) = &mut current {
                id.push_str(&quoted(line));
            }
        } else if (line.starts_with("msgstr") || line.starts_with("msgid_plural"))
            && let Some(id) = current.take()
        {
            result.push(id);
        }
    }
    if let Some(id) = current {
        result.push(id);
    }
    result.into_iter().filter(|id| !id.is_empty()).collect()
}

fn source_ids(dir: &Path) -> HashSet<String> {
    let mut result = HashSet::new();
    for file in std::fs::read_dir(dir).unwrap() {
        let path = file.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "slint") {
            let text = std::fs::read_to_string(path).unwrap();
            for call in text.split("@tr(").skip(1) {
                let call = call.trim_start();
                let first = quoted(call);
                // Also support explicit @tr("context" => "message").
                let mut literals = call.char_indices().skip(1);
                let mut escaped = false;
                let end = literals
                    .find_map(|(i, c)| {
                        let closing = c == '"' && !escaped;
                        escaped = c == '\\' && !escaped;
                        closing.then_some(i + 1)
                    })
                    .unwrap();
                result.insert(match call[end..].trim_start().strip_prefix("=>") {
                    Some(message) => quoted(message.trim_start()),
                    None => first,
                });
            }
        }
    }
    result
}

#[test]
fn hindi_catalog_has_no_stale_messages() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let po =
        std::fs::read_to_string(root.join("translations/hi/LC_MESSAGES/hatchzip-gui.po")).unwrap();
    let sources = source_ids(&root.join("ui"));
    let stale: Vec<_> = catalog_ids(&po)
        .into_iter()
        .filter(|id| !sources.contains(id))
        .collect();
    assert!(
        stale.is_empty(),
        "Hindi catalog contains removed source messages: {stale:?}"
    );
}

#[test]
fn catalog_reader_handles_multiline_and_escaped_messages() {
    let po = "msgid \"\"\n\"Hello \\\"world\\\"\"\nmsgstr \"translation\"\n";
    assert_eq!(catalog_ids(po), ["Hello \"world\""]);
}
