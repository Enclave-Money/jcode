//! `blaude prune` — dead-code *candidates* from the repo graph.
//!
//! GitNexus knows the call graph, so a function with no incoming `CALLS`/
//! `ACCESSES` edge is a candidate for removal. The graph cannot see everything,
//! though: trait methods are dispatched dynamically, macros and `#[test]`
//! harnesses generate calls the parser never sees, and `pub` items may be used
//! from another crate. So this never *asserts* code is dead — it surfaces
//! candidates, filters the obvious false positives structurally, and points at
//! `cargo check` (the compiler's own `dead_code` lint) as ground truth.

use crate::brief;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// One graph-derived candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub file: String,
    pub line: u64,
    pub exported: bool,
}

/// Where a candidate lands after filtering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bucket {
    /// Private, no callers, not a trait/test/FFI symbol — most likely dead.
    High,
    /// Exported or trait-shaped — may be used cross-crate or via dynamic dispatch.
    Low,
    /// A `#[test]`/`#[cfg(test)]` item, `#[allow(dead_code)]`, FFI export, or
    /// `main` — not dead, just invisible to a static call graph.
    Excluded,
}

/// Whether `name` appears as a *call* anywhere in `lines`, ignoring the line
/// that defines it.
///
/// The graph's `CALLS` edges are incomplete in ways that systematically
/// mis-rank live code as dead (see [`classify`]), so before promoting a
/// candidate to [`Bucket::High`] we ask the cheapest possible independent
/// question: does the text of this file contain `name(` somewhere that is not
/// the definition? A textual hit can be a false positive (a same-named method
/// on another type), but that only ever *demotes* a candidate to `Low`, which
/// is the safe direction — prune's job is to avoid confidently pointing at
/// live code.
pub fn has_textual_call_site(lines: &[&str], name: &str, def_idx: usize) -> bool {
    let def = format!("fn {name}");
    lines.iter().enumerate().any(|(i, l)| {
        if i == def_idx || l.contains(&def) {
            return false; // the definition itself is not a call site
        }
        // A mention inside a comment is prose, not a call. Only whole-line
        // comments are skipped; a trailing `// ...` after real code is rare
        // enough that treating it as code merely demotes to `Low`.
        let t = l.trim_start();
        if t.starts_with("//") {
            return false;
        }
        let mut rest = *l;
        while let Some(pos) = rest.find(name) {
            let (before, after) = (&rest[..pos], &rest[pos + name.len()..]);
            // A call is `name(` with a non-identifier char before it, so
            // `open_shell(` matches but `reopen_shell(` does not.
            let boundary_before = before
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_');
            if boundary_before && after.trim_start().starts_with('(') {
                return true;
            }
            rest = after;
        }
        false
    })
}

/// Trait methods Rust dispatches dynamically; the parser rarely records a call
/// edge to them, so "no callers" means nothing here.
const TRAIT_METHODS: &[&str] = &[
    "fmt",
    "drop",
    "default",
    "clone",
    "clone_from",
    "eq",
    "ne",
    "cmp",
    "partial_cmp",
    "hash",
    "from",
    "into",
    "try_from",
    "try_into",
    "as_ref",
    "as_mut",
    "borrow",
    "borrow_mut",
    "deref",
    "deref_mut",
    "index",
    "index_mut",
    "add",
    "sub",
    "mul",
    "div",
    "neg",
    "not",
    "next",
    "next_back",
    "size_hint",
    "poll",
    "poll_next",
    "source",
    "description",
    "cause",
    "to_string",
    "to_owned",
    "from_str",
    "serialize",
    "deserialize",
    "visit_str",
    "visit_map",
    "visit_seq",
    "type_id",
    "provide",
    "call",
    "call_mut",
    "call_once",
    "start",
    "run",
];

/// Find the 0-based line of `fn <name>` in the file, using the graph's
/// (possibly stale) `hint` line as a starting guess. The graph's line numbers
/// drift as soon as an agent edits above a function, so trusting them blindly
/// would read attributes off the wrong lines; relocating by name keeps prune
/// accurate between reindexes.
pub fn locate_fn(lines: &[&str], name: &str, hint_1based: u64) -> usize {
    let needle = format!("fn {name}");
    let is_def = |l: &str| {
        let t = l.trim_start();
        // `fn name(` / `fn name<` / `fn name ` — a definition, not a call or a
        // substring of a longer identifier. Any leading qualifiers (pub/async/
        // unsafe/const/extern) are fine because we search for `fn name` within
        // the line and then check the character that follows the name.
        match t.find(&needle) {
            Some(pos) => {
                let after = &t[pos + needle.len()..];
                matches!(after.chars().next(), Some('(') | Some('<') | Some(' '))
            }
            None => false,
        }
    };
    let hint = (hint_1based as usize).saturating_sub(1);
    if lines.get(hint).map(|l| is_def(l)).unwrap_or(false) {
        return hint;
    }
    // Nearest definition to the hint wins (files can hold same-named methods).
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_def(l))
        .min_by_key(|(i, _)| (*i as i64 - hint as i64).abs())
        .map(|(i, _)| i)
        .unwrap_or(hint)
}

/// Whether the function at `fn_idx` is dynamically dispatched — declared in a
/// `trait` block or defined in an `impl <Trait> for <Type>`. Such methods are
/// invoked through a vtable, so the static call graph records no edge to them
/// and "no callers" is meaningless. Found by scanning up to the nearest
/// enclosing block opener at a lower indentation than the function.
pub fn enclosing_dynamic_dispatch(lines: &[&str], fn_idx: usize) -> bool {
    let indent = |l: &str| l.len() - l.trim_start().len();
    let Some(fn_line) = lines.get(fn_idx) else {
        return false;
    };
    let fn_indent = indent(fn_line);
    if fn_indent == 0 {
        return false; // a column-0 fn is free-standing, never a trait method
    }
    for l in lines[..fn_idx].iter().rev() {
        if l.trim().is_empty() {
            continue;
        }
        if indent(l) >= fn_indent {
            continue; // sibling / inner line, keep climbing
        }
        // The nearest opener at a lower indent is this function's scope.
        let mut t = l.trim_start();
        for kw in [
            "pub(crate) ",
            "pub(super) ",
            "pub ",
            "unsafe ",
            "default ",
            "async ",
        ] {
            t = t.strip_prefix(kw).unwrap_or(t);
        }
        let opens = |t: &str, kw: &str| {
            t.strip_prefix(kw)
                .is_some_and(|r| r.starts_with([' ', '<']) || r.is_empty())
        };
        if opens(t, "trait") {
            return true;
        }
        if opens(t, "impl") {
            return t.contains(" for "); // trait impl (dynamic) vs inherent (static)
        }
        // Any other enclosing opener (mod/fn/etc.) means it is not a trait method.
        return false;
    }
    false
}

/// Classify a candidate given the source lines of its file. Pure so it can be
/// unit-tested without GitNexus. Relocates the function by name first, so a
/// stale graph line number cannot make it read the wrong attributes.
///
/// The last gate is [`has_textual_call_site`]. The graph's `CALLS` edges have
/// two blind spots that both hit *same-file, private* functions — exactly the
/// shape that would otherwise score `High`:
///
/// 1. **Name collision with a struct field.** A method `App::mods()` and a
///    field `App.mods` produce two nodes; the call edge attaches to the
///    `Property`, leaving the `Function` node with no incoming edge.
/// 2. **Calls in argument position.** `f(g(x))` records the outer call but not
///    the inner one, so `g` looks uncalled.
///
/// Both make prune's confidence signal *inverted*: live code ranks High while
/// genuinely dead code sits in Low. Corroborating against the file's own text
/// costs one pass over lines we have already read and removes both classes.
pub fn classify(cand: &Candidate, file_lines: &[&str]) -> Bucket {
    let idx = locate_fn(file_lines, &cand.name, cand.line);

    // Contiguous attributes / doc-comments directly above the fn.
    let mut attrs: Vec<&str> = Vec::new();
    let mut i = idx;
    while i > 0 {
        let l = file_lines[i - 1].trim();
        if l.starts_with("#[") || l.starts_with("#![") {
            attrs.push(l);
            i -= 1;
        } else if l.is_empty() || l.starts_with("//") || l.starts_with("///") {
            i -= 1;
        } else {
            break;
        }
    }
    let attr_has = |needle: &str| attrs.iter().any(|a| a.contains(needle));

    // A `#[test]` / `#[tokio::test]` attribute, or living inside a
    // `#[cfg(test)]` module (indented fn + an earlier cfg(test) in the file).
    let direct_test = attrs
        .iter()
        .any(|a| a.ends_with("test]") || a.contains("test)]"));
    let indented = file_lines
        .get(idx)
        .map(|l| l.starts_with(char::is_whitespace))
        .unwrap_or(false);
    // Inside a `#[cfg(test)]` module only if the nearest such attribute above
    // has not been closed by a column-0 `}` before this function. Without this,
    // any function after the first `#[cfg(test)]` in the file would be treated
    // as a test and dropped — hiding real dead code.
    let cfg_test_above = file_lines[..idx]
        .iter()
        .rposition(|l| l.contains("#[cfg(test)]"))
        .map(|cfg| !file_lines[cfg + 1..idx].iter().any(|l| l.starts_with('}')))
        .unwrap_or(false);
    let is_test = direct_test || (indented && cfg_test_above);

    let is_path_test = cand.file.contains("/tests/")
        || cand.file.contains("/test/")
        || cand.file.starts_with("tests/")
        || cand.file.starts_with("test/")
        || cand.file.ends_with("_test.rs")
        || cand.file.ends_with("_tests.rs")
        || cand
            .file
            .rsplit('/')
            .next()
            .map(|b| b == "tests.rs" || b == "test.rs")
            .unwrap_or(false);

    let allow_dead = attr_has("dead_code") || attr_has("allow(unused");
    let ffi = attr_has("no_mangle")
        || attr_has("export_name")
        || file_lines
            .get(idx)
            .map(|l| l.contains("extern \"") || l.contains("pub extern"))
            .unwrap_or(false);

    if is_test || is_path_test || allow_dead {
        return Bucket::Excluded;
    }
    if ffi || cand.name == "main" {
        return Bucket::Excluded;
    }
    if enclosing_dynamic_dispatch(file_lines, idx)
        || cand.exported
        || TRAIT_METHODS.contains(&cand.name.as_str())
    {
        return Bucket::Low;
    }
    // The graph says "no callers" — but it is only as good as its edges. A
    // call site in the same file is proof enough that this is not dead.
    if has_textual_call_site(file_lines, &cand.name, idx) {
        return Bucket::Low;
    }
    Bucket::High
}

/// The dead-function query: functions with no incoming CALLS/ACCESSES edge.
const DEAD_CODE_CYPHER: &str = "MATCH (f:Function) \
WHERE NOT EXISTS { MATCH ()-[r:CodeRelation]->(f) WHERE r.type IN ['CALLS','ACCESSES'] } \
RETURN f.name AS name, f.filePath AS file, f.isExported AS exported, f.startLine AS line \
ORDER BY file, line";

/// Parse GitNexus's `cypher` JSON (a markdown table inside `{"markdown": …}`)
/// into candidates. Safe because the selected columns never contain `|`.
pub fn parse_candidates(raw: &str) -> Result<Vec<Candidate>> {
    let v: serde_json::Value = serde_json::from_str(raw).context("cypher output not JSON")?;
    let md = v["markdown"].as_str().unwrap_or_default();
    let mut out = Vec::new();
    for line in md.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cells.len() != 4 {
            continue;
        }
        // Skip the header + separator rows.
        if cells[0] == "name" || cells[0].starts_with("---") {
            continue;
        }
        let Ok(line_no) = cells[3].parse::<u64>() else {
            continue;
        };
        out.push(Candidate {
            name: cells[0].to_string(),
            file: cells[1].to_string(),
            line: line_no,
            exported: cells[2] == "true",
        });
    }
    Ok(out)
}

/// Candidates bucketed by confidence.
#[derive(Debug, Default)]
pub struct Pruned {
    pub high: Vec<Candidate>,
    pub low: Vec<Candidate>,
    pub excluded: usize,
}

/// Run the query, read each candidate's file once, and bucket everything.
pub fn analyze(root: &Path) -> Result<Pruned> {
    let raw = brief::cypher(root, DEAD_CODE_CYPHER)?;
    let candidates = parse_candidates(&raw)?;

    // Read each referenced file at most once.
    let mut file_cache: BTreeMap<String, String> = BTreeMap::new();
    let mut pruned = Pruned::default();
    for cand in candidates {
        let content = file_cache
            .entry(cand.file.clone())
            .or_insert_with(|| std::fs::read_to_string(root.join(&cand.file)).unwrap_or_default());
        let lines: Vec<&str> = content.lines().collect();
        match classify(&cand, &lines) {
            Bucket::High => pruned.high.push(cand),
            Bucket::Low => pruned.low.push(cand),
            Bucket::Excluded => pruned.excluded += 1,
        }
    }
    Ok(pruned)
}

const NOTE: &str = "Candidates from the call graph. Dynamic dispatch, macros, and cross-crate \
use can hide real callers — confirm with `cargo check` (its dead_code warnings) before deleting.";

/// `blaude prune` entry point.
pub fn run(json: bool, summary: bool) -> Result<()> {
    let root = brief::repo_root()?;
    let pruned = analyze(&root)?;
    let stale = !json && brief::is_index_stale(&root);

    if json {
        let to_json = |v: &[Candidate]| {
            v.iter()
                .map(|c| {
                    serde_json::json!({
                        "name": c.name, "file": c.file, "line": c.line, "exported": c.exported
                    })
                })
                .collect::<Vec<_>>()
        };
        let out = serde_json::json!({
            "high_confidence": to_json(&pruned.high),
            "low_confidence": to_json(&pruned.low),
            "excluded": pruned.excluded,
            "note": NOTE,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if summary {
        println!(
            "High: {}  Low: {}  Excluded: {}",
            pruned.high.len(),
            pruned.low.len(),
            pruned.excluded
        );
        return Ok(());
    }

    if pruned.high.is_empty() && pruned.low.is_empty() {
        println!("No dead-code candidates — every function has a caller in the graph.");
        return Ok(());
    }

    let print_group = |title: &str, blurb: &str, items: &[Candidate]| {
        if items.is_empty() {
            return;
        }
        println!("\n{title} ({})  {blurb}", items.len());
        for c in items {
            println!("  {}:{}  {}", c.file, c.line, c.name);
        }
    };
    print_group(
        "High confidence",
        "private, no callers in the graph, not a trait/test/FFI symbol",
        &pruned.high,
    );
    print_group(
        "Low confidence",
        "exported or trait-shaped — may be used cross-crate or via dynamic dispatch",
        &pruned.low,
    );
    if pruned.excluded > 0 {
        println!(
            "\nExcluded ({})  #[test]/#[cfg(test)], #[allow(dead_code)], FFI, or main",
            pruned.excluded
        );
    }
    println!("\n{NOTE}");
    if stale {
        println!("(Index is behind the working tree — run `blaude brief` for the freshest set.)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(name: &str, file: &str, line: u64, exported: bool) -> Candidate {
        Candidate {
            name: name.to_string(),
            file: file.to_string(),
            line,
            exported,
        }
    }

    #[test]
    fn a_plain_private_uncalled_fn_is_high_confidence() {
        let src = ["fn helper() {}", "fn dead_one() {}"];
        assert_eq!(
            classify(&cand("dead_one", "src/x.rs", 2, false), &src),
            Bucket::High
        );
    }

    #[test]
    fn exported_and_trait_methods_are_low_confidence() {
        let src = ["pub fn api() {}"];
        assert_eq!(
            classify(&cand("api", "src/x.rs", 1, true), &src),
            Bucket::Low
        );
        let src2 = ["    fn fmt(&self) {}"];
        assert_eq!(
            classify(&cand("fmt", "src/x.rs", 1, false), &src2),
            Bucket::Low
        );
    }

    #[test]
    fn direct_test_attribute_is_excluded() {
        let src = ["#[test]", "fn checks_something() {}"];
        assert_eq!(
            classify(&cand("checks_something", "src/x.rs", 2, false), &src),
            Bucket::Excluded
        );
        let src2 = ["#[tokio::test]", "async fn checks_async() {}"];
        assert_eq!(
            classify(&cand("checks_async", "src/x.rs", 2, false), &src2),
            Bucket::Excluded
        );
    }

    #[test]
    fn inline_cfg_test_module_fn_is_excluded() {
        // An indented fn under an earlier #[cfg(test)] — the common Rust
        // pattern (`#[cfg(test)] mod tests { fn it_works() {} }`).
        let src = [
            "fn real() {}",
            "#[cfg(test)]",
            "mod tests {",
            "    fn it_works() {}",
            "}",
        ];
        assert_eq!(
            classify(&cand("it_works", "src/x.rs", 4, false), &src),
            Bucket::Excluded
        );
        // But a top-level (column-0) fn in the same file is NOT swallowed.
        assert_eq!(
            classify(&cand("real", "src/x.rs", 1, false), &src),
            Bucket::High
        );
    }

    #[test]
    fn real_dead_code_after_an_early_cfg_test_block_is_not_hidden() {
        // An early `#[cfg(test)] mod helpers { .. }` closes with a column-0 `}`;
        // a genuinely-dead top-level fn after it must still surface as High.
        let src = [
            "#[cfg(test)]",
            "mod helpers {",
            "    fn used_by_tests() {}",
            "}",
            "",
            "fn genuinely_dead() {}",
        ];
        assert_eq!(
            classify(&cand("genuinely_dead", "src/x.rs", 6, false), &src),
            Bucket::High
        );
        // But a fn truly inside a trailing cfg(test) module is still excluded.
        let src2 = [
            "fn real() {}",
            "#[cfg(test)]",
            "mod tests {",
            "    fn a_test() {}",
            "}",
        ];
        assert_eq!(
            classify(&cand("a_test", "src/x.rs", 4, false), &src2),
            Bucket::Excluded
        );
    }

    #[test]
    fn test_path_files_are_excluded() {
        let src = ["fn anything() {}"];
        assert_eq!(
            classify(&cand("anything", "tests/e2e.rs", 1, false), &src),
            Bucket::Excluded
        );
        assert_eq!(
            classify(&cand("anything", "src/foo_tests.rs", 1, false), &src),
            Bucket::Excluded
        );
    }

    #[test]
    fn allow_dead_code_and_ffi_and_main_are_excluded() {
        let allow = ["#[allow(dead_code)]", "fn kept() {}"];
        assert_eq!(
            classify(&cand("kept", "src/x.rs", 2, false), &allow),
            Bucket::Excluded
        );
        let ffi = ["#[no_mangle]", "pub extern \"C\" fn c_entry() {}"];
        assert_eq!(
            classify(&cand("c_entry", "src/x.rs", 2, true), &ffi),
            Bucket::Excluded
        );
        let main = ["fn main() {}"];
        assert_eq!(
            classify(&cand("main", "src/main.rs", 1, false), &main),
            Bucket::Excluded
        );
    }

    #[test]
    fn trait_decls_and_trait_impls_are_low_confidence() {
        let trait_src = ["pub trait Backend {", "    fn id(&self) -> u8;", "}"];
        assert_eq!(
            classify(&cand("id", "src/b.rs", 2, false), &trait_src),
            Bucket::Low
        );
        let impl_src = [
            "impl ApplicationHandler for App {",
            "    fn resumed(&mut self) {}",
            "}",
        ];
        assert_eq!(
            classify(&cand("resumed", "src/a.rs", 2, false), &impl_src),
            Bucket::Low
        );
        // An inherent impl method with no callers stays High (statically dispatched).
        let inherent = ["impl App {", "    fn helper(&self) {}", "}"];
        assert_eq!(
            classify(&cand("helper", "src/a.rs", 2, false), &inherent),
            Bucket::High
        );
    }

    #[test]
    fn locate_fn_survives_stale_line_numbers() {
        let src = ["// header", "fn added_later() {}", "", "fn target() {}"];
        // Graph says line 2, but the fn is actually at line 4 (drift).
        assert_eq!(locate_fn(&src, "target", 2), 3);
        // Exact hint is honored.
        assert_eq!(locate_fn(&src, "added_later", 2), 1);
        // `fn target` is not confused by a call to it.
        let src2 = ["    target();", "fn target() {}"];
        assert_eq!(locate_fn(&src2, "target", 1), 1);
    }

    // --- Regression: the graph's "no callers" signal was inverted for
    // same-file private fns. Each case below scored High before the
    // `has_textual_call_site` gate, despite a live caller in the same file.

    #[test]
    fn a_method_shadowed_by_a_same_named_field_is_not_high_confidence() {
        // GitNexus emits `Function:App.mods#0` *and* `Property:App.mods`, and
        // attaches the call edge to the Property — so the Function node has no
        // incoming CALLS and the graph reports it dead. It is not.
        let src = [
            "struct App {",
            "    mods: ModifiersState,",
            "}",
            "impl App {",
            "    fn mods(&self) -> Mods {",
            "        Mods { shift: self.mods.shift_key() }",
            "    }",
            "    fn window_event(&mut self) {",
            "        let action = self.ui.on_key(&key, self.mods(), self.term_modes());",
            "    }",
            "}",
        ];
        assert_eq!(
            classify(&cand("mods", "term/src/app.rs", 5, false), &src),
            Bucket::Low,
            "a method with a live same-file caller must not rank High"
        );
    }

    #[test]
    fn a_call_in_argument_position_is_not_high_confidence() {
        // `Span::new("● ", backend_dot(..))` — the parser records the outer
        // call but not the nested one, so `backend_dot` looks uncalled.
        let src = [
            "fn backend_dot(id: BackendId, chrome: &Chrome) -> Rgb {",
            "    chrome.ok",
            "}",
            "fn draw_tab_row(scene: &mut Scene) {",
            "    spans: vec![",
            "        Span::new(\"● \", backend_dot(tab.backend, chrome)),",
            "    ],",
            "}",
        ];
        assert_eq!(
            classify(&cand("backend_dot", "term/src/scene.rs", 1, false), &src),
            Bucket::Low,
            "a fn called in argument position must not rank High"
        );
    }

    #[test]
    fn a_genuinely_uncalled_fn_still_ranks_high() {
        // The gate must not swallow real dead code: no call site, no demotion.
        let src = [
            "fn genuinely_dead(x: u8) -> u8 {",
            "    x",
            "}",
            "fn other() {",
            "    println!(\"unrelated\");",
            "}",
        ];
        assert_eq!(
            classify(&cand("genuinely_dead", "src/x.rs", 1, false), &src),
            Bucket::High
        );
    }

    #[test]
    fn textual_call_detection_respects_identifier_boundaries() {
        // A longer identifier that merely *contains* the name is not a call.
        let src = ["fn shell(&self) {}", "fn caller() { self.open_shell(); }"];
        assert!(
            !has_textual_call_site(&src, "shell", 0),
            "`open_shell(` must not count as a call to `shell`"
        );
        // A doc-comment mention is not a call either.
        let src2 = ["/// See helper() for details.", "fn helper() {}"];
        assert!(!has_textual_call_site(&src2, "helper", 1));
        // But a real call is found.
        let src3 = ["fn helper() {}", "fn caller() { helper(1); }"];
        assert!(has_textual_call_site(&src3, "helper", 0));
        // Including one split across lines, as rustfmt writes long arg lists.
        let src4 = [
            "fn helper() {}",
            "fn caller() {",
            "    helper(",
            "        1,",
            "    );",
            "}",
        ];
        assert!(has_textual_call_site(&src4, "helper", 0));
    }

    #[test]
    fn parses_a_cypher_markdown_table() {
        let raw = r#"{"markdown":"| name | file | exported | line |\n| --- | --- | --- | --- |\n| dead_one | src/x.rs | false | 2 |\n| api | src/y.rs | true | 10 |","row_count":2}"#;
        let got = parse_candidates(raw).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], cand("dead_one", "src/x.rs", 2, false));
        assert_eq!(got[1], cand("api", "src/y.rs", 10, true));
    }
}
