//! Tree-sitter based AST extraction engine.
//!
//! Provides accurate structural extraction using native tree-sitter grammars
//! for Python, JavaScript, TypeScript, Rust, Go, Java, C, C++, Ruby, C#, and Dart.
//! Falls back gracefully to the regex-based extractor for unsupported languages.

mod handlers;
mod imports;
mod treesitter_config;

pub use treesitter_config::TsConfig;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphEdge, GraphNode, NodeType};
use tracing::trace;
use tree_sitter::{Language, Node, Parser};

// ═══════════════════════════════════════════════════════════════════════════
// Public entry point
// ═══════════════════════════════════════════════════════════════════════════

/// Try tree-sitter extraction for a supported language.
/// Returns `None` if the language is not supported by tree-sitter grammars.
pub fn try_extract(path: &Path, source: &[u8], lang: &str) -> Option<ExtractionResult> {
    let (language, config) = treesitter_config::resolve_language(lang)?;
    extract_with_treesitter(path, source, language, &config, lang)
}

// ═══════════════════════════════════════════════════════════════════════════
// Core extraction
// ═══════════════════════════════════════════════════════════════════════════

/// Extract graph nodes and edges from a single file using tree-sitter.
fn extract_with_treesitter(
    path: &Path,
    source: &[u8],
    language: Language,
    config: &TsConfig,
    lang: &str,
) -> Option<ExtractionResult> {
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    let str_path = path.to_string_lossy();

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut seen_ids = HashSet::new();
    // For the call-graph pass we record (caller_nid, body_start_byte, body_end_byte)
    let mut function_bodies: Vec<(String, usize, usize)> = Vec::new();

    // File node
    let file_nid = make_id(&[&str_path]);
    seen_ids.insert(file_nid.clone());
    nodes.push(GraphNode {
        id: file_nid.clone(),
        label: stem.to_string(),
        source_file: str_path.to_string(),
        source_location: None,
        node_type: NodeType::File,
        community: None,
        extra: HashMap::new(),
    });

    // Walk the AST
    walk_node(
        root,
        source,
        config,
        lang,
        &file_nid,
        stem,
        &str_path,
        &mut nodes,
        &mut edges,
        &mut seen_ids,
        &mut function_bodies,
        None,
    );

    // ---- Call-graph pass ----
    // Build label → nid mapping for known functions
    let label_to_nid: HashMap<String, String> = nodes
        .iter()
        .filter(|n| matches!(n.node_type, NodeType::Function | NodeType::Method))
        .map(|n| {
            let normalized = n
                .label
                .trim_end_matches("()")
                .trim_start_matches('.')
                .to_lowercase();
            (normalized, n.id.clone())
        })
        .collect();

    let mut seen_calls: HashSet<(String, String)> = HashSet::new();
    for (caller_nid, body_start, body_end) in &function_bodies {
        let body_text = &source[*body_start..*body_end];
        let body_str = String::from_utf8_lossy(body_text);
        let body_lower = body_str.to_lowercase();
        for (func_label, callee_nid) in &label_to_nid {
            if callee_nid == caller_nid {
                continue;
            }
            // Heuristic: look for `func_name(` in body, or for Ruby-style no-parens calls
            let has_paren_call = body_lower.contains(&format!("{func_label}("));
            let has_noparen_call = if lang == "ruby" {
                // Ruby allows `func arg` or `func\n` — check if func_label appears
                // as a standalone word (not part of a longer identifier)
                body_lower.find(func_label.as_str()).is_some_and(|pos| {
                    let after = pos + func_label.len();
                    if after >= body_lower.len() {
                        true // at end of body
                    } else {
                        let next_ch = body_lower.as_bytes()[after];
                        // Must be followed by non-alphanumeric (space, newline, paren, etc.)
                        !next_ch.is_ascii_alphanumeric() && next_ch != b'_'
                    }
                })
            } else {
                false
            };
            if has_paren_call || has_noparen_call {
                let key = (caller_nid.clone(), callee_nid.clone());
                if seen_calls.insert(key) {
                    edges.push(GraphEdge {
                        source: caller_nid.clone(),
                        target: callee_nid.clone(),
                        relation: "calls".to_string(),
                        confidence: Confidence::Inferred,
                        confidence_score: Confidence::Inferred.default_score(),
                        source_file: str_path.to_string(),
                        source_location: None,
                        weight: 1.0,
                        extra: HashMap::new(),
                    });
                }
            }
        }
    }

    trace!(
        "treesitter({}): {} nodes, {} edges from {}",
        lang,
        nodes.len(),
        edges.len(),
        str_path
    );

    Some(ExtractionResult {
        nodes,
        edges,
        hyperedges: vec![],
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// AST walking
// ═══════════════════════════════════════════════════════════════════════════

#[allow(clippy::too_many_arguments)]
pub(crate) fn walk_node(
    node: Node,
    source: &[u8],
    config: &TsConfig,
    lang: &str,
    file_nid: &str,
    stem: &str,
    str_path: &str,
    nodes: &mut Vec<GraphNode>,
    edges: &mut Vec<GraphEdge>,
    seen_ids: &mut HashSet<String>,
    function_bodies: &mut Vec<(String, usize, usize)>,
    parent_class_nid: Option<&str>,
) {
    let kind = node.kind();

    // ---- Imports ----
    if config.import_types.contains(kind) {
        // For Ruby/Elixir, `call` is in multiple type sets.
        // Only treat specific calls as imports; let other calls recurse normally.
        if lang == "ruby" && kind == "call" {
            let method_name = node
                .child_by_field_name("method")
                .map(|n| node_text(n, source))
                .unwrap_or_default();
            if method_name == "require" || method_name == "require_relative" {
                imports::extract_import(node, source, file_nid, str_path, lang, edges, nodes);
                return;
            }
            // Not a require call, fall through to normal processing
        } else if lang == "elixir" && kind == "call" {
            let target = node
                .child_by_field_name(config.name_field)
                .map(|n| node_text(n, source))
                .unwrap_or_default();
            if matches!(target.as_str(), "import" | "use" | "require" | "alias") {
                imports::extract_import(node, source, file_nid, str_path, lang, edges, nodes);
                return;
            }
            // Not an import call — fall through to class/function checks
        } else {
            imports::extract_import(node, source, file_nid, str_path, lang, edges, nodes);
            return; // Don't recurse into import children
        }
    }

    // ---- Classes / Structs / Enums / Traits ----
    if config.class_types.contains(kind) {
        if lang == "elixir" && kind == "call" {
            let target = node
                .child_by_field_name(config.name_field)
                .map(|n| node_text(n, source))
                .unwrap_or_default();
            if target != "defmodule" && target != "defprotocol" && target != "defimpl" {
                // Not a module definition — skip class handling
            } else {
                handlers::handle_class_like(
                    node,
                    source,
                    config,
                    lang,
                    file_nid,
                    stem,
                    str_path,
                    nodes,
                    edges,
                    seen_ids,
                    function_bodies,
                );
                return;
            }
        } else {
        handlers::handle_class_like(
            node,
            source,
            config,
            lang,
            file_nid,
            stem,
            str_path,
            nodes,
            edges,
            seen_ids,
            function_bodies,
        );
        return;
        }
    }

    // ---- Functions / Methods ----
    if config.function_types.contains(kind) {
        if lang == "elixir" && kind == "call" {
            let target = node
                .child_by_field_name(config.name_field)
                .map(|n| node_text(n, source))
                .unwrap_or_default();
            if matches!(target.as_str(), "def" | "defp" | "defmacro" | "defmacrop" | "defguard" | "defguardp" | "defdelegate") {
                handlers::handle_function(
                    node,
                    source,
                    config,
                    lang,
                    file_nid,
                    stem,
                    str_path,
                    nodes,
                    edges,
                    seen_ids,
                    function_bodies,
                    parent_class_nid,
                );
                return;
            }
            // Not a function definition — fall through to recursion
        } else {
        handlers::handle_function(
            node,
            source,
            config,
            lang,
            file_nid,
            stem,
            str_path,
            nodes,
            edges,
            seen_ids,
            function_bodies,
            parent_class_nid,
        );
        return;
        }
    }

    // ---- Default: recurse into children ----
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_node(
            child,
            source,
            config,
            lang,
            file_nid,
            stem,
            str_path,
            nodes,
            edges,
            seen_ids,
            function_bodies,
            parent_class_nid,
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════

#[allow(clippy::too_many_arguments)]
pub(crate) fn node_text(node: Node, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or("").to_string()
}

/// Get the name of a definition node via its field name.
pub(crate) fn get_name(node: Node, source: &[u8], field: &str) -> Option<String> {
    let name_node = node.child_by_field_name(field)?;
    // For C/C++ declarators, unwrap nested declarators to find the identifier
    let text = unwrap_declarator_name(name_node, source);
    if text.is_empty() { None } else { Some(text) }
}

/// Recursively unwrap C/C++ declarators (function_declarator, pointer_declarator, etc.)
/// to find the underlying identifier name.
pub(crate) fn unwrap_declarator_name(node: Node, source: &[u8]) -> String {
    match node.kind() {
        "function_declarator"
        | "pointer_declarator"
        | "reference_declarator"
        | "parenthesized_declarator" => {
            // The actual name is in the "declarator" field or first named child
            if let Some(inner) = node.child_by_field_name("declarator") {
                return unwrap_declarator_name(inner, source);
            }
            // Fallback: look for an identifier child
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "identifier" || child.kind() == "field_identifier" {
                    return node_text(child, source);
                }
            }
            node_text(node, source)
        }
        "qualified_identifier" | "scoped_identifier" => {
            // C++ qualified names like `Foo::bar` — use the "name" field
            if let Some(name) = node.child_by_field_name("name") {
                return node_text(name, source);
            }
            node_text(node, source)
        }
        _ => node_text(node, source),
    }
}


pub(crate) fn make_edge(
    source_id: &str,
    target_id: &str,
    relation: &str,
    source_file: &str,
    line: usize,
) -> GraphEdge {
    GraphEdge {
        source: source_id.to_string(),
        target: target_id.to_string(),
        relation: relation.to_string(),
        confidence: Confidence::Extracted,
        confidence_score: Confidence::Extracted.default_score(),
        source_file: source_file.to_string(),
        source_location: Some(format!("L{line}")),
        weight: 1.0,
        extra: HashMap::new(),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

// Tests moved to tests/treesitter.rs (integration tests)
