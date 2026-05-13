//! dbt project extraction.
//!
//! Invokes `dbt compile`, parses `manifest.json`, and extracts column-level lineage
//! from compiled SQL files.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphEdge, GraphNode, NodeType};
use graphify_detect::DbtProject;
use tracing::{debug, warn};
use walkdir::WalkDir;

/// Extract all dbt projects into a combined [`ExtractionResult`].
///
/// For each project in `projects`:
/// 1. Runs `dbt compile` in the project root to produce a fresh `manifest.json`.
///    If the `dbt` binary is not found, that project is skipped with a warning.
///    If compilation fails, extraction continues with whatever manifest already exists.
/// 2. Parses `target/manifest.json`, emitting [`NodeType::Relation`] nodes for every
///    `model`, `seed`, `snapshot`, and `source`, together with `defines`, `part_of`,
///    and `depends_on` edges.
/// 3. Walks `target/compiled/**/*.sql` and feeds each file through the plain-SQL
///    extractor to collect column-level lineage (`derives_from` edges).
///
/// Projects whose manifest is absent after compilation are skipped with a warning.
pub fn extract_dbt_projects(projects: &[DbtProject]) -> ExtractionResult {
    let mut combined = ExtractionResult::default();

    for project in projects {
        debug!("running dbt compile for project '{}'", project.name);

        // Run dbt compile; skip the project if the binary is not found, proceed
        // with whatever manifest exists if compilation fails.
        let output = match Command::new("dbt")
            .arg("compile")
            .current_dir(&project.root)
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                warn!("failed to invoke dbt compile for '{}': {}", project.name, e);
                continue;
            }
        };
        if !output.status.success() {
            warn!(
                "dbt compile failed for '{}', proceeding with available manifest",
                project.name
            );
        }

        let manifest_path = project.root.join("target/manifest.json");
        if !manifest_path.exists() {
            warn!(
                "no manifest.json found for '{}' (expected at {})",
                project.name,
                manifest_path.display()
            );
            continue;
        }

        let manifest_content = match fs::read_to_string(&manifest_path) {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to read manifest.json for '{}': {}", project.name, e);
                continue;
            }
        };

        let manifest: serde_json::Value = match serde_json::from_str(&manifest_content) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "failed to parse manifest.json for '{}': {}",
                    project.name, e
                );
                continue;
            }
        };

        let mut result = parse_manifest(&project.name, &manifest, &project.root);

        // Also process compiled SQL files for column lineage and FKs
        let compiled_dir = project.root.join("target/compiled");
        if compiled_dir.exists() {
            for entry in WalkDir::new(&compiled_dir).into_iter().flatten() {
                if entry.file_type().is_file()
                    && entry.path().extension().and_then(|e| e.to_str()) == Some("sql")
                {
                    let source = fs::read_to_string(entry.path()).unwrap_or_default();
                    let sql_res = crate::sql::extract_sql(entry.path(), &source);

                    // We only want the Columns and Expressions from compiled sql, and their derives_from edges.
                    // The SQL extractor also emits Application, File, and Relation nodes, which might be duplicates.
                    // But `make_id` keeps them deterministic, so build() will deduplicate them.
                    result.nodes.extend(sql_res.nodes);
                    result.edges.extend(sql_res.edges);
                }
            }
        }

        combined.nodes.extend(result.nodes);
        combined.edges.extend(result.edges);
    }

    combined
}

fn parse_manifest(app_name: &str, manifest: &serde_json::Value, root: &Path) -> ExtractionResult {
    let mut result = ExtractionResult::default();

    let app_id = make_id(&["app", app_name]);
    result.nodes.push(GraphNode {
        id: app_id.clone(),
        label: app_name.to_string(),
        source_file: root.join("dbt_project.yml").to_string_lossy().into_owned(),
        source_location: None,
        node_type: NodeType::Application,
        community: None,
        extra: HashMap::new(),
    });

    let mut defined_models = HashMap::new();

    if let Some(nodes) = manifest.get("nodes").and_then(|n| n.as_object()) {
        for (node_id, node) in nodes {
            let resource_type = node
                .get("resource_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if resource_type != "model" && resource_type != "seed" && resource_type != "snapshot" {
                continue;
            }

            let name = node.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let schema = node.get("schema").and_then(|v| v.as_str()).unwrap_or("");
            let rel_id = make_id(&["rel", app_name, schema, name]);

            defined_models.insert(node_id.clone(), rel_id.clone());

            let mut extra = HashMap::new();
            extra.insert("relation_kind".to_string(), serde_json::json!("dbt_model"));
            if let Some(desc) = node.get("description") {
                extra.insert("description".to_string(), desc.clone());
            }

            let original_path = node
                .get("original_file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let source_file = root.join(original_path).to_string_lossy().into_owned();

            result.nodes.push(GraphNode {
                id: rel_id.clone(),
                label: if schema.is_empty() {
                    name.to_string()
                } else {
                    format!("{}.{}", schema, name)
                },
                source_file: source_file.clone(),
                source_location: None,
                node_type: NodeType::Relation,
                community: None,
                extra,
            });

            // File node
            let file_id = make_id(&["file", &source_file.replace('/', "_")]);
            result.nodes.push(GraphNode {
                id: file_id.clone(),
                label: Path::new(&source_file)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                source_file: source_file.clone(),
                source_location: None,
                node_type: NodeType::File,
                community: None,
                extra: HashMap::new(),
            });

            // defines edge
            result.edges.push(GraphEdge {
                source: file_id.clone(),
                target: rel_id.clone(),
                relation: "defines".to_string(),
                confidence: Confidence::Extracted,
                confidence_score: 1.0,
                source_file: source_file.clone(),
                source_location: None,
                weight: 1.0,
                extra: HashMap::new(),
            });

            // part_of edge
            result.edges.push(GraphEdge {
                source: rel_id.clone(),
                target: app_id.clone(),
                relation: "part_of".to_string(),
                confidence: Confidence::Extracted,
                confidence_score: 1.0,
                source_file,
                source_location: None,
                weight: 1.0,
                extra: HashMap::new(),
            });

            // depends_on edges are resolved in pass 2 below once all nodes are registered.
        }
    }

    if let Some(sources) = manifest.get("sources").and_then(|s| s.as_object()) {
        for (node_id, node) in sources {
            let name = node.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let schema = node.get("schema").and_then(|v| v.as_str()).unwrap_or("");
            let rel_id = make_id(&["rel", app_name, schema, name]);

            defined_models.insert(node_id.clone(), rel_id.clone());

            let mut extra = HashMap::new();
            extra.insert("relation_kind".to_string(), serde_json::json!("source"));

            let original_path = node
                .get("original_file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let source_file = root.join(original_path).to_string_lossy().into_owned();

            result.nodes.push(GraphNode {
                id: rel_id.clone(),
                label: if schema.is_empty() {
                    name.to_string()
                } else {
                    format!("{}.{}", schema, name)
                },
                source_file: source_file.clone(),
                source_location: None,
                node_type: NodeType::Relation,
                community: None,
                extra,
            });

            result.edges.push(GraphEdge {
                source: rel_id.clone(),
                target: app_id.clone(),
                relation: "part_of".to_string(),
                confidence: Confidence::Extracted,
                confidence_score: 1.0,
                source_file,
                source_location: None,
                weight: 1.0,
                extra: HashMap::new(),
            });
        }
    }

    // Pass 2: resolve depends_on edges
    if let Some(nodes) = manifest.get("nodes").and_then(|n| n.as_object()) {
        for (node_id, node) in nodes {
            if let Some(source_rel_id) = defined_models.get(node_id)
                && let Some(depends_on) = node
                    .get("depends_on")
                    .and_then(|d| d.get("nodes"))
                    .and_then(|n| n.as_array())
            {
                for dep in depends_on.iter().filter_map(|d| d.as_str()) {
                    if let Some(target_rel_id) = defined_models.get(dep) {
                        result.edges.push(GraphEdge {
                            source: source_rel_id.clone(),
                            target: target_rel_id.clone(),
                            relation: "depends_on".to_string(),
                            confidence: Confidence::Extracted,
                            confidence_score: 1.0,
                            source_file: "manifest.json".to_string(),
                            source_location: None,
                            weight: 1.0,
                            extra: HashMap::new(),
                        });
                    }
                }
            }
        }
    }

    result
}

// NOTE: DBT tests require mocking the dbt CLI and filesystem. For integration
// testing at the extract() level, a full dbt project setup is needed.
// These unit tests verify the core extraction logic in isolation.
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_manifest() {
        let manifest = json!({
            "nodes": {
                "model.my_app.my_model": {
                    "resource_type": "model",
                    "name": "my_model",
                    "schema": "public",
                    "original_file_path": "models/my_model.sql",
                    "description": "A test model",
                    "depends_on": {
                        "nodes": ["source.my_app.my_source"]
                    }
                }
            },
            "sources": {
                "source.my_app.my_source": {
                    "resource_type": "source",
                    "name": "my_source",
                    "schema": "raw",
                    "original_file_path": "models/sources.yml"
                }
            }
        });

        let result = parse_manifest("my_app", &manifest, Path::new("/my_app"));

        let app_node = result
            .nodes
            .iter()
            .find(|n| n.node_type == NodeType::Application)
            .unwrap();
        assert_eq!(app_node.label, "my_app");

        let model_node = result
            .nodes
            .iter()
            .find(|n| n.label == "public.my_model")
            .unwrap();
        assert_eq!(model_node.extra.get("relation_kind").unwrap(), "dbt_model");

        let source_node = result
            .nodes
            .iter()
            .find(|n| n.label == "raw.my_source")
            .unwrap();
        assert_eq!(source_node.extra.get("relation_kind").unwrap(), "source");

        let depends_on = result
            .edges
            .iter()
            .find(|e| e.relation == "depends_on")
            .unwrap();
        assert_eq!(depends_on.source, model_node.id);
        assert_eq!(depends_on.target, source_node.id);
    }

    #[test]
    fn test_extract_dbt_projects() {
        use graphify_detect::DbtProject;
        use std::collections::HashSet;
        use tempfile::tempdir;

        // Skip if dbt is not available in PATH
        if std::process::Command::new("dbt")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping test_extract_dbt_projects: dbt not in PATH");
            return;
        }

        let dir = tempdir().unwrap();
        let target_dir = dir.path().join("target");
        std::fs::create_dir_all(&target_dir).unwrap();

        let manifest = json!({
            "nodes": {
                "model.my_app.my_model": {
                    "resource_type": "model",
                    "name": "my_model",
                    "schema": "public",
                    "original_file_path": "models/my_model.sql"
                }
            }
        });

        std::fs::write(target_dir.join("manifest.json"), manifest.to_string()).unwrap();

        // Write a fake compiled SQL
        let compiled_dir = target_dir.join("compiled");
        std::fs::create_dir_all(&compiled_dir).unwrap();
        std::fs::write(compiled_dir.join("my_model.sql"), "SELECT a FROM b").unwrap();

        let proj = DbtProject {
            root: dir.path().to_path_buf(),
            name: "my_app".to_string(),
            model_paths: vec![],
            snapshot_paths: vec![],
            managed_sql_paths: HashSet::new(),
        };

        let result = extract_dbt_projects(&[proj]);

        // Should extract Application, Relation, File from manifest
        let app_node = result
            .nodes
            .iter()
            .find(|n| n.node_type == NodeType::Application);
        assert!(app_node.is_some());

        // Also it should process the SQL file (even if tree-sitter fails to fully parse, we get something)
        // since tree-sitter-sequel is available, it should parse SELECT a FROM b.
        // It will emit dependencies from compiled SQL too!
        // At the very least, nodes should be non-empty
        assert!(!result.nodes.is_empty());
    }

    // Fix 18 — Missing dbt CLI / non-existent project root should not panic.
    #[test]
    fn test_extract_dbt_projects_missing_cli() {
        use std::collections::HashSet;

        // A project whose root does not exist: dbt compile cannot run, the manifest
        // will not be present, and the project is skipped with a warning.
        // This must not panic under any circumstances.
        let proj = DbtProject {
            root: std::path::PathBuf::from("/nonexistent/path/that/does/not/exist"),
            name: "missing_cli_test".to_string(),
            model_paths: vec![],
            snapshot_paths: vec![],
            managed_sql_paths: HashSet::new(),
        };

        let result = extract_dbt_projects(&[proj]);
        // Either empty or contains no Application nodes produced by this project.
        assert!(
            result.nodes.is_empty()
                || !result
                    .nodes
                    .iter()
                    .any(|n| n.node_type == NodeType::Application && n.label == "missing_cli_test"),
            "Missing CLI / root should produce empty or near-empty results"
        );
    }

    // Fix 20 — Cross-project depends_on: dependency on undefined target is not emitted
    // within a single manifest parse; stub creation happens at resolve_sql_cross_file.
    #[test]
    fn test_parse_manifest_cross_project_depends_on() {
        let manifest_a = json!({
            "nodes": {
                "model.app_a.orders_report": {
                    "resource_type": "model",
                    "name": "orders_report",
                    "schema": "analytics",
                    "original_file_path": "models/orders_report.sql",
                    "depends_on": {
                        // Cross-project dependency — this node is not defined in manifest_a.
                        "nodes": ["source.app_b.raw_orders"]
                    }
                }
            }
        });

        let result_a = parse_manifest("app_a", &manifest_a, std::path::Path::new("/repo/app_a"));

        // The depends_on edge target references a source that doesn’t exist in this manifest.
        // Within a single parse_manifest call, the edge should NOT be created for undefined
        // targets — cross-project resolution happens at the resolve_sql_cross_file level.
        let dep = result_a.edges.iter().find(|e| e.relation == "depends_on");
        assert!(
            dep.is_none(),
            "depends_on to undefined cross-project target should not create an edge within a \
             single manifest parse; got {:?}",
            dep
        );
    }

    // C9 — Monorepo: two separate `parse_manifest` calls with different app names produce
    // independent Application nodes and app-scoped Relation IDs.
    #[test]
    fn test_parse_manifest_monorepo_multiple_applications() {
        let manifest_a = json!({
            "nodes": {
                "model.app_a.table_a": {
                    "resource_type": "model",
                    "name": "table_a",
                    "schema": "schema_a",
                    "original_file_path": "models/table_a.sql"
                }
            }
        });
        let manifest_b = json!({
            "nodes": {
                "model.app_b.table_b": {
                    "resource_type": "model",
                    "name": "table_b",
                    "schema": "schema_b",
                    "original_file_path": "models/table_b.sql"
                }
            }
        });

        let result_a = parse_manifest("app_a", &manifest_a, Path::new("/repo/app_a"));
        let result_b = parse_manifest("app_b", &manifest_b, Path::new("/repo/app_b"));

        // Each result must have its own distinct Application node.
        let app_a = result_a
            .nodes
            .iter()
            .find(|n| n.node_type == NodeType::Application)
            .expect("app_a should have an Application node");
        let app_b = result_b
            .nodes
            .iter()
            .find(|n| n.node_type == NodeType::Application)
            .expect("app_b should have an Application node");

        assert_eq!(app_a.label, "app_a");
        assert_eq!(app_b.label, "app_b");
        assert_ne!(
            app_a.id, app_b.id,
            "Application IDs must differ across apps"
        );

        // Relation nodes must be labelled correctly and scoped to their app.
        let rel_a = result_a
            .nodes
            .iter()
            .find(|n| n.node_type == NodeType::Relation)
            .expect("app_a should have a Relation node");
        let rel_b = result_b
            .nodes
            .iter()
            .find(|n| n.node_type == NodeType::Relation)
            .expect("app_b should have a Relation node");

        assert_eq!(rel_a.label, "schema_a.table_a");
        assert_eq!(rel_b.label, "schema_b.table_b");
        assert_ne!(
            rel_a.id, rel_b.id,
            "Relation IDs must be scoped to their app"
        );

        // Each Relation must be part_of its own Application.
        let a_part_of = result_a
            .edges
            .iter()
            .find(|e| e.relation == "part_of" && e.source == rel_a.id && e.target == app_a.id);
        assert!(a_part_of.is_some(), "table_a should be part_of app_a");

        let b_part_of = result_b
            .edges
            .iter()
            .find(|e| e.relation == "part_of" && e.source == rel_b.id && e.target == app_b.id);
        assert!(b_part_of.is_some(), "table_b should be part_of app_b");
    }
}
