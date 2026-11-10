//! Keep the catalog and marked source messages in sync, including plurals.
use std::collections::HashSet;
use std::path::Path;

fn literal(text: &str) -> (String, &str) {
    // Both PO and the messages here use quoted strings with escaped quotes.
    let mut escaped = false;
    for (i, c) in text.char_indices().skip(1) {
        if c == '"' && !escaped {
            return (
                serde_json::from_str(&text[..=i]).expect("valid message literal"),
                &text[i + 1..],
            );
        }
        escaped = c == '\\' && !escaped;
    }
    panic!("unterminated message: {text}");
}

fn quoted(text: &str) -> String {
    literal(text).0
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
        } else if let Some(rest) = line.strip_prefix("msgid_plural ") {
            if let Some(id) = current.take() {
                result.push(id);
            }
            current = Some(quoted(rest));
        } else if line.starts_with('"') {
            if let Some(id) = &mut current {
                id.push_str(&quoted(line));
            }
        } else if line.starts_with("msgstr")
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

fn source_ids_in(text: &str) -> HashSet<String> {
    let mut result = HashSet::new();
    for call in text.split("@tr(").skip(1) {
        let (first, rest) = literal(call.trim_start());
        let (message, rest) = match rest.trim_start().strip_prefix("=>") {
            Some(message) => literal(message.trim_start()),
            None => (first, rest),
        };
        result.insert(message);
        if let Some(plural) = rest.trim_start().strip_prefix('|') {
            result.insert(quoted(plural.trim_start()));
        }
    }
    result
}

fn source_ids(dir: &Path) -> HashSet<String> {
    let mut result = HashSet::new();
    for file in std::fs::read_dir(dir).unwrap() {
        let path = file.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "slint") {
            result.extend(source_ids_in(&std::fs::read_to_string(path).unwrap()));
        }
    }
    result
}

#[test]
fn hindi_catalog_matches_marked_messages() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let po =
        std::fs::read_to_string(root.join("translations/hi/LC_MESSAGES/hatchzip-gui.po")).unwrap();
    let sources = source_ids(&root.join("ui"));
    let catalog: HashSet<_> = catalog_ids(&po).into_iter().collect();
    let stale: Vec<_> = catalog.iter().filter(|id| !sources.contains(*id)).collect();
    assert!(
        stale.is_empty(),
        "Hindi catalog contains removed source messages: {stale:?}"
    );
    // Newly marked messages await Hindi translations; keep checking the
    // completeness of the messages this catalog already covered.
    let untranslated = HashSet::from([
        "GB",
        "Sufficient disk space",
        "Insufficient disk space. {} more is required.",
        "Removing the archive during extraction requires only {}.",
        "There is also enough space to keep the archive.",
        "Most of the required space is one solid block, which can only be released after it is fully extracted.",
        "The destination is on another drive, so removing the archive does not free space there.",
        "This drive cannot release space inside a file, so the compressed archive remains whole until extraction completes.",
        "Required: {}. Available: {}.",
        "Archive {} of {}",
        "All {} archives completed.",
        "1 more archive has not started",
        "{} more archives have not started",
        "Available: {}. Required: {}, plus {} reserved.",
        "Version {}",
        "Extracts large archives in place. The archive is removed progressively during extraction, so only the space for the extracted files is required.",
        "Licensed under MIT or Apache-2.0",
        "Details: {}",
        "Test cancelled. The archive was not modified.",
        "Extraction cancelled. The archive was not modified.",
    ]);
    let missing: Vec<_> = sources
        .difference(&catalog)
        .filter(|id| !untranslated.contains(id.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "Hindi catalog is missing messages: {missing:?}"
    );
}

#[test]
fn chinese_catalog_matches_marked_messages() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let po = std::fs::read_to_string(root.join("translations/zh_CN/LC_MESSAGES/hatchzip-gui.po"))
        .unwrap();
    let sources = source_ids(&root.join("ui"));
    let catalog: HashSet<_> = catalog_ids(&po).into_iter().collect();
    assert_eq!(catalog, sources);
}

#[test]
fn catalog_reader_handles_multiline_and_escaped_messages() {
    let po = "msgid \"\"\n\"Hello \\\"world\\\"\"\nmsgstr \"translation\"\n";
    assert_eq!(catalog_ids(po), ["Hello \"world\""]);
}

#[test]
fn plural_messages_are_checked_on_both_sides() {
    let po = "msgid \"one\"\nmsgid_plural \"many\"\nmsgstr[0] \"a\"\nmsgstr[1] \"b\"\n";
    assert_eq!(catalog_ids(po), ["one", "many"]);
    assert_eq!(
        source_ids_in("@tr(\"one\" | \"many\" % n)"),
        HashSet::from(["one".into(), "many".into()])
    );
    assert_eq!(
        source_ids_in("@tr(\"context\" => \"one\" | \"many\" % n)"),
        HashSet::from(["one".into(), "many".into()])
    );
}
