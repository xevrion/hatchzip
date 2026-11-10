//! Keep every translation catalog in sync with the marked source messages.
//!
//! Each folder in `translations/` is checked the same way, so a new language
//! needs no change here: its messages must match the `@tr(...)` texts in
//! `ui/`, keep every placeholder, have one form per plural the language's
//! `Plural-Forms` header names, and none may be empty or marked fuzzy.
use std::collections::HashSet;
use std::path::Path;

/// Source messages a catalog may leave out. `GB` falls back to English on
/// purpose, which also exercises the fallback.
const EXEMPT: &[&str] = &["GB"];

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

/// One catalog entry. The header is the entry with an empty `msgid`.
#[derive(Debug, Default)]
struct Entry {
    msgid: String,
    plural: Option<String>,
    msgstr: Vec<String>,
    fuzzy: bool,
}

/// Which string a continuation line (`"..."`) belongs to.
enum Field {
    Id,
    Plural,
    Str,
}

fn entries(po: &str) -> Vec<Entry> {
    let mut result = Vec::new();
    let mut current: Option<Entry> = None;
    let mut field = Field::Id;
    let mut fuzzy = false;
    for line in po.lines().map(str::trim) {
        if line.starts_with("#,") {
            fuzzy |= line.contains("fuzzy");
        } else if let Some(rest) = line.strip_prefix("msgid ") {
            result.extend(current.take());
            current = Some(Entry {
                msgid: quoted(rest),
                fuzzy,
                ..Entry::default()
            });
            fuzzy = false;
            field = Field::Id;
        } else if let Some(rest) = line.strip_prefix("msgid_plural ") {
            if let Some(e) = &mut current {
                e.plural = Some(quoted(rest));
            }
            field = Field::Plural;
        } else if let Some(rest) = line.strip_prefix("msgstr") {
            // `msgstr "..."` or `msgstr[n] "..."`.
            let text = &rest[rest.find('"').expect("quoted msgstr")..];
            if let Some(e) = &mut current {
                e.msgstr.push(quoted(text));
            }
            field = Field::Str;
        } else if line.starts_with('"')
            && let Some(e) = &mut current
        {
            let more = quoted(line);
            match field {
                Field::Id => e.msgid.push_str(&more),
                Field::Plural => e.plural.get_or_insert_default().push_str(&more),
                Field::Str => e.msgstr.last_mut().expect("msgstr").push_str(&more),
            }
        }
    }
    result.extend(current);
    result
}

/// The msgids of a catalog, singular and plural, without the header.
fn catalog_ids(po: &str) -> Vec<String> {
    entries(po)
        .into_iter()
        .filter(|e| !e.msgid.is_empty())
        .flat_map(|e| std::iter::once(e.msgid).chain(e.plural))
        .collect()
}

/// A header field such as `Language` or `Plural-Forms`.
fn header<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header
        .lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
        .map(str::trim)
}

fn nplurals(forms: &str) -> usize {
    forms
        .split(';')
        .find_map(|p| p.trim().strip_prefix("nplurals="))
        .and_then(|n| n.trim().parse().ok())
        .expect("Plural-Forms names nplurals")
}

/// Which arguments a message uses, sorted. `{}` takes the next argument and
/// `{1}` names one, so a translation may reorder them: `{} of {}` and
/// `{1} / {0}` both use arguments 0 and 1.
fn placeholders(text: &str) -> Vec<usize> {
    let mut found = Vec::new();
    let mut next = 0;
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) if after[..end].bytes().all(|b| b.is_ascii_digit()) => {
                let index = if end == 0 {
                    next += 1;
                    next - 1
                } else {
                    after[..end].parse().unwrap()
                };
                found.push(index);
                rest = &after[end + 1..];
            }
            _ => rest = after,
        }
    }
    found.sort_unstable();
    found
}

/// Whether `part` only uses placeholders `whole` has, as an earlier plural
/// form may leave the number out ("1 entry" in a language's singular).
fn subset(part: &[usize], whole: &[usize]) -> bool {
    let mut left = whole.to_vec();
    part.iter().all(|p| match left.iter().position(|w| w == p) {
        Some(i) => {
            left.remove(i);
            true
        }
        None => false,
    })
}

fn edges(text: &str) -> (bool, bool) {
    (
        text.starts_with(char::is_whitespace),
        text.ends_with(char::is_whitespace),
    )
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

/// Everything wrong with one catalog, as readable lines.
fn problems(language: &str, po: &str, sources: &HashSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    let all = entries(po);
    let Some(head) = all.iter().find(|e| e.msgid.is_empty()) else {
        return vec![format!("{language}: no header entry")];
    };
    let head = head.msgstr.first().map(String::as_str).unwrap_or_default();
    if header(head, "Language") != Some(language) {
        out.push(format!(
            "{language}: the Language header is {:?}, not the folder name",
            header(head, "Language")
        ));
    }
    let forms = match header(head, "Plural-Forms") {
        Some(forms) => nplurals(forms),
        None => {
            out.push(format!("{language}: no Plural-Forms header"));
            return out;
        }
    };
    let ids: HashSet<_> = catalog_ids(po).into_iter().collect();
    for id in ids.iter().filter(|id| !sources.contains(*id)) {
        out.push(format!(
            "{language}: {id:?} is not marked in the source any more"
        ));
    }
    for id in sources.difference(&ids) {
        if !EXEMPT.contains(&id.as_str()) {
            out.push(format!("{language}: {id:?} has no translation"));
        }
    }
    for e in all.iter().filter(|e| !e.msgid.is_empty()) {
        let id = &e.msgid;
        if e.fuzzy {
            out.push(format!("{language}: {id:?} is marked fuzzy"));
        }
        if e.msgstr.iter().any(String::is_empty) {
            out.push(format!("{language}: {id:?} has an empty translation"));
        }
        match &e.plural {
            None => {
                let s = e.msgstr.first().map(String::as_str).unwrap_or_default();
                if placeholders(s) != placeholders(id) {
                    out.push(format!(
                        "{language}: {id:?} has placeholders {:?}, the source {:?}",
                        placeholders(s),
                        placeholders(id)
                    ));
                }
                if !s.is_empty() && edges(s) != edges(id) {
                    out.push(format!(
                        "{language}: {id:?} starts or ends with different whitespace"
                    ));
                }
            }
            Some(plural) => {
                if e.msgstr.len() != forms {
                    out.push(format!(
                        "{language}: {id:?} has {} plural forms, Plural-Forms says {forms}",
                        e.msgstr.len()
                    ));
                }
                let whole = placeholders(plural);
                if let Some((last, earlier)) = e.msgstr.split_last() {
                    if placeholders(last) != whole {
                        out.push(format!(
                            "{language}: the last form of {plural:?} has placeholders {:?}, \
                             the source {whole:?}",
                            placeholders(last)
                        ));
                    }
                    for form in earlier {
                        if !subset(&placeholders(form), &whole) {
                            out.push(format!(
                                "{language}: a form of {id:?} has a placeholder the source lacks"
                            ));
                        }
                    }
                }
            }
        }
    }
    out
}

/// The language folders under `translations/`.
fn languages(root: &Path) -> Vec<String> {
    let mut found: Vec<_> = std::fs::read_dir(root.join("translations"))
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    found
}

#[test]
fn every_catalog_matches_marked_messages() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sources = source_ids(&root.join("ui"));
    let languages = languages(root);
    assert!(!languages.is_empty(), "no translations found");
    let mut all = Vec::new();
    for language in &languages {
        let path = root.join(format!(
            "translations/{language}/LC_MESSAGES/hatchzip-gui.po"
        ));
        match std::fs::read_to_string(&path) {
            Ok(po) => all.extend(problems(language, &po, &sources)),
            Err(e) => all.push(format!("{language}: reading {}: {e}", path.display())),
        }
    }
    assert!(
        all.is_empty(),
        "translation catalogs need fixing:\n{}",
        all.join("\n")
    );
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

const HEADER_TWO: &str = "msgid \"\"\nmsgstr \"\"\n\"Language: xx\\n\"\n\"Plural-Forms: nplurals=2; plural=(n != 1);\\n\"\n";
const HEADER_ONE: &str =
    "msgid \"\"\nmsgstr \"\"\n\"Language: xx\\n\"\n\"Plural-Forms: nplurals=1; plural=0;\\n\"\n";

fn check(po: &str, sources: &[&str]) -> Vec<String> {
    let sources = sources.iter().map(|s| (*s).to_string()).collect();
    problems("xx", po, &sources)
}

#[test]
fn a_lost_placeholder_is_caught() {
    let po = format!("{HEADER_TWO}msgid \"Open {{}}\"\nmsgstr \"Abrir\"\n");
    assert_eq!(check(&po, &["Open {}"]).len(), 1);
}

#[test]
fn reordered_placeholders_are_fine() {
    let po = format!("{HEADER_TWO}msgid \"{{}} of {{}}\"\nmsgstr \"{{1}} / {{0}}\"\n");
    assert!(check(&po, &["{} of {}"]).is_empty());
    let po = format!("{HEADER_TWO}msgid \"{{}} of {{}}\"\nmsgstr \"{{1}} / {{1}}\"\n");
    assert_eq!(check(&po, &["{} of {}"]).len(), 1);
}

#[test]
fn one_plural_form_is_enough_where_the_language_has_one() {
    let po = format!(
        "{HEADER_ONE}msgid \"{{}} file\"\nmsgid_plural \"{{}} files\"\nmsgstr[0] \"{{}} 个文件\"\n"
    );
    assert!(check(&po, &["{} file", "{} files"]).is_empty());
}

#[test]
fn a_missing_plural_form_is_caught() {
    let po = format!(
        "{HEADER_TWO}msgid \"{{}} file\"\nmsgid_plural \"{{}} files\"\nmsgstr[0] \"{{}} fichero\"\n"
    );
    assert_eq!(check(&po, &["{} file", "{} files"]).len(), 1);
}

#[test]
fn stale_missing_empty_and_fuzzy_messages_are_caught() {
    let po = format!(
        "{HEADER_TWO}msgid \"Old\"\nmsgstr \"Viejo\"\n#, fuzzy\nmsgid \"Close\"\nmsgstr \"\"\n"
    );
    let found = check(&po, &["Close", "New"]);
    assert_eq!(found.len(), 4, "{found:#?}");
}
