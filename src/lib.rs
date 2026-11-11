//! Generic fallback parser - full-parse mode.
//!
//! This parser intentionally does not depend on a host tree-sitter grammar. It
//! builds a small, stable semantic tree from source lines, simple brace blocks,
//! and token leaves so the advertised `generic` language can satisfy the
//! supported-language contract without pretending to understand a real grammar.

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}

struct GenericParser;

/// One non-blank source row. `indent` is the UTF-8 byte column where the
/// trimmed `text` starts in the raw row, so `indent + text.len()` is the byte
/// column just past its last non-whitespace character.
#[derive(Debug, Clone)]
struct SourceLine {
    number: u32,
    text: String,
    indent: u32,
}

impl SourceLine {
    fn end_col(&self) -> u32 {
        self.indent + self.text.len() as u32
    }
}

#[derive(Debug, Clone)]
struct Block {
    node_type: String,
    label: String,
    signature: SourceLine,
    body: Vec<SourceLine>,
    end_line: u32,
    end_col: u32,
}

#[derive(Debug, Clone)]
enum Item {
    Block(Block),
    Line(SourceLine),
}

fn clean_line(line: &str) -> String {
    line.trim().to_string()
}

/// Byte length of the leading whitespace (UTF-8 byte column of the first
/// non-whitespace character), never a character count.
fn line_indent(line: &str) -> u32 {
    (line.len() - line.trim_start().len()) as u32
}

/// Rows of `source` split on LF only. A trailing CR (CRLF files) stays in the
/// raw row; it is trimmed away from node text, so it never moves a column.
fn source_rows(source: &str) -> impl Iterator<Item = &str> {
    source.split('\n')
}

/// Exact end of source: row = number of LFs, column = UTF-8 byte length of
/// the text after the last LF.
fn source_end(source: &str) -> (u32, u32) {
    let rows = source.bytes().filter(|b| *b == b'\n').count() as u32;
    let last = source.rfind('\n').map(|i| i + 1).unwrap_or(0);
    (rows, (source.len() - last) as u32)
}

fn braces_delta(line: &str) -> i32 {
    let mut delta = 0;
    let mut in_string: Option<char> = None;
    let mut escaped = false;
    for ch in line.chars() {
        if let Some(quote) = in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            in_string = Some(ch);
        } else if ch == '{' {
            delta += 1;
        } else if ch == '}' {
            delta -= 1;
        }
    }
    delta
}

fn function_label(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix("function ")?;
    let name: String = rest
        .chars()
        .take_while(|ch| ch.is_alphanumeric() || *ch == '_' || *ch == '$')
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn block_kind_and_label(trimmed: &str) -> (String, String) {
    if let Some(name) = function_label(trimmed) {
        return ("function_declaration".to_string(), name);
    }
    if let Some((lhs, _)) = trimmed.split_once('=') {
        let label = lhs.trim();
        if !label.is_empty() {
            return ("object_declaration".to_string(), label.to_string());
        }
    }
    ("block".to_string(), "block".to_string())
}

fn parse_items(source: &str) -> Vec<Item> {
    let mut items = Vec::new();
    let mut current: Option<Block> = None;
    let mut depth: i32 = 0;

    for (idx, raw_line) in source_rows(source).enumerate() {
        let line_no = idx as u32;
        let trimmed = clean_line(raw_line);
        if trimmed.is_empty() {
            continue;
        }
        let line = SourceLine {
            number: line_no,
            text: trimmed,
            indent: line_indent(raw_line),
        };

        if let Some(block) = current.as_mut() {
            depth += braces_delta(&line.text);
            block.end_line = line.number;
            block.end_col = line.end_col();
            if depth <= 0 && line.text == "}" {
                items.push(Item::Block(block.clone()));
                current = None;
                depth = 0;
            } else {
                block.body.push(line);
                if depth <= 0 {
                    items.push(Item::Block(block.clone()));
                    current = None;
                    depth = 0;
                }
            }
            continue;
        }

        let delta = braces_delta(&line.text);
        if delta > 0 {
            let (node_type, label) = block_kind_and_label(&line.text);
            current = Some(Block {
                node_type,
                label,
                end_line: line.number,
                end_col: line.end_col(),
                signature: line,
                body: Vec::new(),
            });
            depth = delta;
        } else {
            items.push(Item::Line(line));
        }
    }

    if let Some(block) = current {
        items.push(Item::Block(block));
    }
    items
}

fn token_spans(text: &str) -> Vec<(String, u32, u32)> {
    let mut spans = Vec::new();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (start_byte, ch) = chars[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            let quote = ch;
            let mut end = i + 1;
            let mut escaped = false;
            while end < chars.len() {
                let (_, current) = chars[end];
                if escaped {
                    escaped = false;
                } else if current == '\\' {
                    escaped = true;
                } else if current == quote {
                    end += 1;
                    break;
                }
                end += 1;
            }
            let end_byte = if end < chars.len() {
                chars[end].0
            } else {
                text.len()
            };
            spans.push((
                text[start_byte..end_byte].to_string(),
                start_byte as u32,
                end_byte as u32,
            ));
            i = end;
            continue;
        }
        if ch.is_alphanumeric() || ch == '_' || ch == '$' {
            let mut end = i + 1;
            while end < chars.len() {
                let (_, current) = chars[end];
                if !(current.is_alphanumeric() || current == '_' || current == '$') {
                    break;
                }
                end += 1;
            }
            let end_byte = if end < chars.len() {
                chars[end].0
            } else {
                text.len()
            };
            spans.push((
                text[start_byte..end_byte].to_string(),
                start_byte as u32,
                end_byte as u32,
            ));
            i = end;
        } else {
            let end_byte = start_byte + ch.len_utf8();
            spans.push((ch.to_string(), start_byte as u32, end_byte as u32));
            i += 1;
        }
    }
    spans
}

fn token_node(
    id: &str,
    line: &SourceLine,
    index: usize,
    token: &(String, u32, u32),
) -> SemanticNode {
    SemanticNodeBuilder::new(
        format!("{}.{}", id, index),
        "token",
        &token.0,
        line.number,
        line.indent + token.1,
        line.number,
        line.indent + token.2,
        "",
    )
    .build()
}

fn line_node(id: &str, line: &SourceLine) -> SemanticNode {
    let node_type = if line.text.starts_with("return ") {
        "return_statement"
    } else if line.text.contains('=') {
        "assignment_statement"
    } else {
        "statement"
    };
    let tokens = token_spans(&line.text);
    let children = tokens
        .iter()
        .enumerate()
        .map(|(i, token)| token_node(id, line, i, token))
        .collect();
    SemanticNodeBuilder::new(
        id,
        node_type,
        &line.text,
        line.number,
        line.indent,
        line.number,
        line.end_col(),
        "",
    )
    .children(children)
    .build()
}

fn block_node(id: &str, block: &Block) -> SemanticNode {
    let mut children = vec![line_node(&format!("{}.0", id), &block.signature)];
    for (index, line) in block.body.iter().enumerate() {
        children.push(line_node(&format!("{}.{}", id, index + 1), line));
    }
    SemanticNodeBuilder::new(
        id,
        &block.node_type,
        &block.label,
        block.signature.number,
        block.signature.indent,
        block.end_line,
        block.end_col,
        "",
    )
    .children(children)
    .build()
}

fn process_impl(source: &str) -> String {
    let items = parse_items(source);
    let children: Vec<SemanticNode> = items
        .iter()
        .enumerate()
        .map(|(index, item)| match item {
            Item::Block(block) => block_node(&format!("0.{}", index), block),
            Item::Line(line) => line_node(&format!("0.{}", index), line),
        })
        .collect();
    let (end_row, end_col) = source_end(source);
    let sem = SemanticNodeBuilder::new(
        "0",
        "generic_file",
        "generic_file",
        0,
        0,
        end_row,
        end_col,
        "",
    )
    .children(children)
    .build();
    serde_json::to_string(&sem).unwrap_or_else(|e| format!(r#"{{"error":"Serialisation: {}"}}"#, e))
}

impl Guest for GenericParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "generic".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        if filename.is_empty() {
            String::new()
        } else {
            "generic".to_string()
        }
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "function processData(input) {\n    result = transform(input)\n    return result\n}\n\nconfig = {\n    timeout: 30,\n    retries: 3\n}\n".to_string(),
            new: "function processData(input, options) {\n    validated = validate(input)\n    result = transform(validated, options)\n    log(\"Processing complete\")\n    return result\n}\n\nconfig = {\n    timeout: 60,\n    retries: 5,\n    verbose: true\n}\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        Vec::new()
    }
    fn language_ids() -> Vec<String> {
        vec!["generic".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        -10
    }
}

export!(GenericParser);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!GenericParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = GenericParser::grammar_id();
        let ids = GenericParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert!(matches!(
            GenericParser::get_parser_mode(),
            ParserMode::FullParse
        ));
    }

    #[test]
    fn detect_language_nonempty_filename() {
        let r = GenericParser::detect_language("test.txt".to_string(), "".to_string());
        assert_eq!(r.as_str(), "generic");
    }

    #[test]
    fn detect_language_empty_filename_returns_empty() {
        let r = GenericParser::detect_language("".to_string(), "".to_string());
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }

    #[test]
    fn playground_example_produces_structured_generic_nodes() {
        let example = <GenericParser as Guest>::example("generic".to_string());
        let out = process_impl(&example.new);
        t::assert_valid_json(&out, "generic example");
        t::assert_no_error(&out, "generic example");
        t::assert_contains_node_type(&out, "generic_file", "generic example");
        t::assert_contains_node_type(&out, "function_declaration", "generic example");
        assert!(
            out.contains("processData"),
            "expected generic example function label: {}",
            out
        );
    }

    // ---- Source-position contract (issue #4): 0-based rows, UTF-8 byte
    // columns, root spans (0,0) to exact EOF, children inside parents. ----

    fn rows(source: &str) -> Vec<&str> {
        source.split('\n').collect()
    }

    fn pos(node: &serde_json::Value) -> (u64, u64, u64, u64) {
        let p = &node["position"];
        (
            p["start_line"].as_u64().unwrap(),
            p["start_col"].as_u64().unwrap(),
            p["end_line"].as_u64().unwrap(),
            p["end_col"].as_u64().unwrap(),
        )
    }

    fn check_point(source: &str, row: u64, col: u64, what: &str) {
        let rows = rows(source);
        assert!(
            (row as usize) < rows.len(),
            "{what}: row {row} out of bounds"
        );
        let line = rows[row as usize];
        assert!(col as usize <= line.len(), "{what}: col {col} past row end");
        assert!(
            line.is_char_boundary(col as usize),
            "{what}: col {col} inside a UTF-8 sequence"
        );
    }

    fn check_node(
        source: &str,
        node: &serde_json::Value,
        parent: Option<(u64, u64, u64, u64)>,
        ids: &mut std::collections::HashSet<String>,
    ) {
        let id = node["id"].as_str().unwrap().to_string();
        assert!(ids.insert(id.clone()), "duplicate id {id}");
        let (sr, sc, er, ec) = pos(node);
        check_point(source, sr, sc, &format!("{id} start"));
        check_point(source, er, ec, &format!("{id} end"));
        assert!((sr, sc) <= (er, ec), "{id}: start after end");
        if let Some((psr, psc, per, pec)) = parent {
            assert!(
                (psr, psc) <= (sr, sc) && (er, ec) <= (per, pec),
                "{id}: child outside parent range"
            );
        }
        for child in node["children"].as_array().unwrap() {
            check_node(source, child, Some((sr, sc, er, ec)), ids);
        }
    }

    fn tree(source: &str) -> serde_json::Value {
        let out = process_impl(source);
        t::assert_no_error(&out, "generic positions");
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        let mut ids = std::collections::HashSet::new();
        check_node(source, &value, None, &mut ids);
        value
    }

    fn root_span(source: &str) -> (u64, u64, u64, u64) {
        pos(&tree(source))
    }

    #[test]
    fn root_spans_exact_source_extent() {
        assert_eq!(root_span(""), (0, 0, 0, 0));
        assert_eq!(root_span("x = 1"), (0, 0, 0, 5));
        assert_eq!(root_span("x = 1\n"), (0, 0, 1, 0));
        assert_eq!(root_span("a\nb\n\n"), (0, 0, 3, 0));
        assert_eq!(root_span("   \n  "), (0, 0, 1, 2));
        assert_eq!(root_span("\u{2003}x = 1"), (0, 0, 0, 8));
    }

    #[test]
    fn web_generic_incomplete_root_is_in_bounds() {
        // Exact old/new pair of core's `web-generic-incomplete` assessment case.
        let old = "The upload is accepted when (";
        let new = "The upload is accepted unless (";
        assert_eq!(root_span(old), (0, 0, 0, old.len() as u64));
        assert_eq!(root_span(new), (0, 0, 0, new.len() as u64));
    }

    #[test]
    fn multibyte_indentation_uses_byte_columns() {
        // U+2003 EM SPACE is 3 UTF-8 bytes; issue #4 reproduction.
        let value = tree("\u{2003}x = 1");
        let stmt = &value["children"][0];
        assert_eq!(stmt["label"], "x = 1");
        assert_eq!(pos(stmt), (0, 3, 0, 8));
        assert_eq!(pos(&stmt["children"][0]), (0, 3, 0, 4));
        assert_eq!(pos(&stmt["children"][2]), (0, 7, 0, 8));
    }

    #[test]
    fn multibyte_tokens_use_byte_columns() {
        let value = tree("na\u{ef}ve = \"caf\u{e9}\" \u{2192} 1");
        let tokens = value["children"][0]["children"].as_array().unwrap();
        assert_eq!(tokens[0]["label"], "na\u{ef}ve");
        assert_eq!(pos(&tokens[0]), (0, 0, 0, 6));
        assert_eq!(pos(&tokens[2]), (0, 9, 0, 16));
        assert_eq!(tokens[3]["label"], "\u{2192}");
        assert_eq!(pos(&tokens[3]), (0, 17, 0, 20));
    }

    #[test]
    fn crlf_rows_keep_cr_out_of_node_columns() {
        // Convention: rows split on LF; a CR before LF is part of the raw row
        // but never inside a node span. EOF column counts raw bytes.
        let source = "a = 1\r\nb = 2\r\n";
        let value = tree(source);
        assert_eq!(pos(&value), (0, 0, 2, 0));
        assert_eq!(pos(&value["children"][0]), (0, 0, 0, 5));
        assert_eq!(pos(&value["children"][1]), (1, 0, 1, 5));
        assert_eq!(root_span("a = 1\r"), (0, 0, 0, 6));
    }

    #[test]
    fn adjacent_blocks_span_from_signature_to_closing_brace() {
        let source = "  function f(x) {\n    return x\n  }\ncfg = {\n  a: 1\n}";
        let value = tree(source);
        let blocks = value["children"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["node_type"], "function_declaration");
        assert_eq!(pos(&blocks[0]), (0, 2, 2, 3));
        assert_eq!(pos(&blocks[0]["children"][0]), (0, 2, 0, 17));
        assert_eq!(blocks[1]["node_type"], "object_declaration");
        assert_eq!(pos(&blocks[1]), (3, 0, 5, 1));
        assert_eq!(pos(&value), (0, 0, 5, 1));
    }

    #[test]
    fn unclosed_block_ends_at_its_last_line() {
        let value = tree("x = {\n  a: 1\n");
        let block = &value["children"][0];
        assert_eq!(pos(block), (0, 0, 1, 6));
        assert_eq!(pos(&value), (0, 0, 2, 0));
    }

    #[test]
    fn playground_example_positions_are_valid() {
        let example = <GenericParser as Guest>::example("generic".to_string());
        tree(&example.old);
        tree(&example.new);
    }
}
