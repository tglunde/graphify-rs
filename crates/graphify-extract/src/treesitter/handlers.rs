//! Tree-sitter node handler functions.

use std::collections::{HashMap, HashSet};

use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{GraphEdge, GraphNode, NodeType};
use tree_sitter::Node;

use super::treesitter_config::TsConfig;
use super::{get_name, make_edge, node_text, walk_node};

pub(crate) fn handle_class_like(
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
) {
    let kind = node.kind();

    // For Go type_declaration, we need to dig into the type_spec child
    if lang == "go" && kind == "type_declaration" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "type_spec" {
                handle_go_type_spec(
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
                );
            }
        }
        return;
    }

    // Rust impl_item: extract methods inside, create "implements" edges
    if lang == "rust" && kind == "impl_item" {
        handle_rust_impl(
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

    // Standard class/struct/enum/trait
    let class_field = config.class_name_field.unwrap_or(config.name_field);
    let name = match get_name(node, source, class_field) {
        Some(n) => n,
        None => return,
    };
    let line = node.start_position().row + 1;
    let class_nid = make_id(&[str_path, &name]);

    let node_type = classify_class_kind(kind, lang);

    if seen_ids.insert(class_nid.clone()) {
        nodes.push(GraphNode {
            id: class_nid.clone(),
            label: name.clone(),
            source_file: str_path.to_string(),
            source_location: Some(format!("L{line}")),
            node_type,
            community: None,
            extra: HashMap::new(),
        });
        edges.push(make_edge(file_nid, &class_nid, "defines", str_path, line));
    }

    // Recurse into body to find methods
    if let Some(body) = node.child_by_field_name(config.body_field) {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
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
                Some(&class_nid),
            );
        }
    }
}

pub(crate) fn classify_class_kind(kind: &str, lang: &str) -> NodeType {
    match kind {
        // Rust
        "struct_item" => NodeType::Struct,
        "enum_item" => NodeType::Enum,
        "trait_item" => NodeType::Trait,
        // C / C++
        "struct_specifier" => NodeType::Struct,
        "enum_specifier" => NodeType::Enum,
        "namespace_definition" => NodeType::Namespace,
        // C#
        "struct_declaration" => NodeType::Struct,
        "enum_declaration" => match lang {
            "csharp" | "java" | "dart" => NodeType::Enum,
            _ => NodeType::Enum,
        },
        // Java / C#
        "interface_declaration" => NodeType::Interface,
        // Dart
        "mixin_declaration" | "extension_declaration" => NodeType::Class,
        // Ruby
        "module" => NodeType::Module,
        // C (type_definition is used for typedef'd structs/enums)
        "type_definition" => NodeType::Struct,
        // Default
        _ => NodeType::Class,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_go_type_spec(
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
) {
    let name = match get_name(node, source, "name") {
        Some(n) => n,
        None => return,
    };
    let line = node.start_position().row + 1;
    let nid = make_id(&[str_path, &name]);

    // Determine struct vs interface by looking at the type child
    let node_type = {
        let mut nt = NodeType::Struct;
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "interface_type" => {
                    nt = NodeType::Interface;
                    break;
                }
                "struct_type" => {
                    nt = NodeType::Struct;
                    break;
                }
                _ => {}
            }
        }
        nt
    };

    if seen_ids.insert(nid.clone()) {
        nodes.push(GraphNode {
            id: nid.clone(),
            label: name.clone(),
            source_file: str_path.to_string(),
            source_location: Some(format!("L{line}")),
            node_type,
            community: None,
            extra: HashMap::new(),
        });
        edges.push(make_edge(file_nid, &nid, "defines", str_path, line));
    }

    // Recurse into body for any child methods (Go doesn't nest methods in struct body,
    // but interfaces have method specs)
    if let Some(body) = node.child_by_field_name(config.body_field) {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
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
                Some(&nid),
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_rust_impl(
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
) {
    // `impl [Trait for] Type { ... }`
    // The type is the `type` field, the trait is the `trait` field
    let type_name = node
        .child_by_field_name("type")
        .map(|n| node_text(n, source));
    let trait_name = node
        .child_by_field_name("trait")
        .map(|n| node_text(n, source));

    let impl_target_nid = type_name.as_ref().map(|tn| make_id(&[str_path, tn]));

    // Create an "implements" edge if trait impl
    if let (Some(trait_n), Some(target_nid)) = (&trait_name, &impl_target_nid) {
        let line = node.start_position().row + 1;
        let trait_nid = make_id(&[str_path, trait_n]);
        edges.push(GraphEdge {
            source: target_nid.clone(),
            target: trait_nid,
            relation: "implements".to_string(),
            confidence: Confidence::Extracted,
            confidence_score: Confidence::Extracted.default_score(),
            source_file: str_path.to_string(),
            source_location: Some(format!("L{line}")),
            weight: 1.0,
            extra: HashMap::new(),
        });
    }

    // Recurse into body to find methods, treating them as methods of the impl target
    if let Some(body) = node.child_by_field_name(config.body_field) {
        let class_nid = impl_target_nid.as_deref();
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
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
                class_nid,
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Function handler
// ═══════════════════════════════════════════════════════════════════════════

#[allow(clippy::too_many_arguments)]
/// Normalize Dart function names: handle getters/setters and named constructors
pub(crate) fn normalize_dart_function_name(lang: &str, func_name: &str) -> String {
    if lang != "dart" {
        return func_name.to_string();
    }

    let mut name = func_name;

    // Strip "get "/"set " prefix for Dart getters/setters (e.g., "get value" -> "value")
    if name.starts_with("get ") || name.starts_with("set ") {
        name = &name[4..];
    }

    name.to_string()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_function(
    node: Node,
    source: &[u8],
    config: &TsConfig,
    _lang: &str,
    file_nid: &str,
    _stem: &str,
    str_path: &str,
    nodes: &mut Vec<GraphNode>,
    edges: &mut Vec<GraphEdge>,
    seen_ids: &mut HashSet<String>,
    function_bodies: &mut Vec<(String, usize, usize)>,
    parent_class_nid: Option<&str>,
) {
    // For JS arrow functions assigned to a variable, the name is on the parent
    // `variable_declarator` node. But for function_declaration, method_definition,
    // etc., the name is directly on the node.
    let func_name = match get_name(node, source, config.name_field) {
        Some(n) => n,
        None => {
            // For JS arrow functions, try to get name from parent variable_declarator
            if node.kind() == "arrow_function" {
                if let Some(parent) = node.parent() {
                    if parent.kind() == "variable_declarator" {
                        match get_name(parent, source, "name") {
                            Some(n) => n,
                            None => return,
                        }
                    } else {
                        return;
                    }
                } else {
                    return;
                }
            } else if _lang == "dart" {
                // Dart function_signature/method_signature may not have a name field;
                // try to find the first identifier child as the function name
                let mut found = None;
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "identifier" {
                        found = Some(node_text(child, source));
                        break;
                    }
                }
                match found {
                    Some(n) if !n.is_empty() => n,
                    _ => return,
                }
            } else {
                return;
            }
        }
    };

    let normalized_name = normalize_dart_function_name(_lang, &func_name);
    let line = node.start_position().row + 1;

    let (func_nid, label, node_type, relation) = if let Some(class_nid) = parent_class_nid {
        let nid = make_id(&[class_nid, &normalized_name]);
        (
            nid,
            format!(".{normalized_name}()"),
            NodeType::Method,
            "defines",
        )
    } else {
        let nid = make_id(&[str_path, &normalized_name]);
        (
            nid,
            format!("{normalized_name}()"),
            NodeType::Function,
            "defines",
        )
    };

    if seen_ids.insert(func_nid.clone()) {
        nodes.push(GraphNode {
            id: func_nid.clone(),
            label,
            source_file: str_path.to_string(),
            source_location: Some(format!("L{line}")),
            node_type,
            community: None,
            extra: HashMap::new(),
        });

        let parent_nid = parent_class_nid.unwrap_or(file_nid);
        edges.push(make_edge(parent_nid, &func_nid, relation, str_path, line));
    }

    // Record the function body bytes for call-graph inference
    if let Some(body) = node.child_by_field_name(config.body_field) {
        function_bodies.push((func_nid, body.start_byte(), body.end_byte()));
    } else {
        // Fallback: use the whole node as body
        function_bodies.push((func_nid, node.start_byte(), node.end_byte()));
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Import handler
// ═══════════════════════════════════════════════════════════════════════════

