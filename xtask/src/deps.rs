use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::process::Command;

use serde::Deserialize;

/// Crates that must stay pure logic (ADR-0001).
const PURE_CRATES: &[&str] = &["bridge-protocol", "bridge-core"];

const FORBIDDEN: &[&str] = &[
    "bridge-transport",
    "bridge-discovery",
    "bridge-platform",
    "bridge-ipc",
    "bridge-daemon",
    "tokio",
    "async-std",
    "smol",
    "mio",
    "socket2",
    "quinn",
    "quinn-proto",
    "rustls",
    "mdns-sd",
    "windows",
    "windows-sys",
    "objc2",
    "x11rb",
    "nix",
    "alsa",
    "midir",
];

#[derive(Deserialize)]
pub(crate) struct Metadata {
    packages: Vec<Package>,
    resolve: Option<Resolve>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
    deps: Vec<NodeDep>,
}

#[derive(Deserialize)]
struct NodeDep {
    pkg: String,
    dep_kinds: Vec<DepKind>,
}

#[derive(Deserialize)]
struct DepKind {
    kind: Option<String>,
}

pub(crate) fn run(root: &Path) -> Result<(), String> {
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["metadata", "--format-version", "1"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let meta: Metadata = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("cannot parse cargo metadata: {e}"))?;

    let found = violations(&meta, PURE_CRATES, FORBIDDEN)?;
    if found.is_empty() {
        println!(
            "check-deps: {} are free of forbidden dependencies",
            PURE_CRATES.join(", ")
        );
        return Ok(());
    }
    for chain in &found {
        eprintln!("forbidden dependency: {}", chain.join(" -> "));
    }
    Err(format!("{} forbidden dependency path(s)", found.len()))
}

/// Returns one dependency chain per forbidden crate reachable from a pure crate
/// through normal (non-dev, non-build) edges.
pub(crate) fn violations(
    meta: &Metadata,
    roots: &[&str],
    forbidden: &[&str],
) -> Result<Vec<Vec<String>>, String> {
    let resolve = meta
        .resolve
        .as_ref()
        .ok_or("cargo metadata has no resolve graph")?;
    let names: HashMap<&str, &str> = meta
        .packages
        .iter()
        .map(|p| (p.id.as_str(), p.name.as_str()))
        .collect();
    let edges: HashMap<&str, Vec<&str>> = resolve
        .nodes
        .iter()
        .map(|n| {
            let normal = n
                .deps
                .iter()
                .filter(|d| d.dep_kinds.iter().any(|k| k.kind.is_none()))
                .map(|d| d.pkg.as_str())
                .collect();
            (n.id.as_str(), normal)
        })
        .collect();

    let mut chains = Vec::new();
    for root in roots {
        let Some(start) = meta.packages.iter().find(|p| p.name == *root) else {
            return Err(format!("package {root} not found in workspace"));
        };
        let mut parent: HashMap<&str, &str> = HashMap::new();
        let mut queue = VecDeque::from([start.id.as_str()]);
        parent.insert(start.id.as_str(), "");
        while let Some(id) = queue.pop_front() {
            let name = names.get(id).copied().unwrap_or(id);
            if id != start.id && forbidden.contains(&name) {
                let mut chain = vec![name.to_owned()];
                let mut cur = parent[id];
                while !cur.is_empty() {
                    chain.push(names.get(cur).copied().unwrap_or(cur).to_owned());
                    cur = parent[cur];
                }
                chain.reverse();
                chains.push(chain);
                continue;
            }
            for next in edges.get(id).into_iter().flatten() {
                if !parent.contains_key(next) {
                    parent.insert(next, id);
                    queue.push_back(next);
                }
            }
        }
    }
    Ok(chains)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(json: &str) -> Metadata {
        serde_json::from_str(json).unwrap()
    }

    const GRAPH: &str = r#"{
        "packages": [
            {"id": "core", "name": "bridge-core"},
            {"id": "proto", "name": "bridge-protocol"},
            {"id": "serde", "name": "serde"},
            {"id": "helper", "name": "helper"},
            {"id": "tokio", "name": "tokio"},
            {"id": "proptest", "name": "proptest"}
        ],
        "resolve": {"nodes": [
            {"id": "core", "deps": [
                {"pkg": "proto", "dep_kinds": [{"kind": null}]},
                {"pkg": "proptest", "dep_kinds": [{"kind": "dev"}]}
            ]},
            {"id": "proto", "deps": [{"pkg": "serde", "dep_kinds": [{"kind": null}]}]},
            {"id": "serde", "deps": []},
            {"id": "helper", "deps": [{"pkg": "tokio", "dep_kinds": [{"kind": null}]}]},
            {"id": "tokio", "deps": []},
            {"id": "proptest", "deps": [{"pkg": "tokio", "dep_kinds": [{"kind": null}]}]}
        ]}
    }"#;

    #[test]
    fn clean_graph_passes_and_dev_edges_are_ignored() {
        let m = meta(GRAPH);
        let found = violations(&m, &["bridge-core", "bridge-protocol"], &["tokio"]).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn transitive_forbidden_dependency_is_reported_with_chain() {
        let graph = GRAPH.replace(
            r#"{"id": "serde", "deps": []}"#,
            r#"{"id": "serde", "deps": [{"pkg": "helper", "dep_kinds": [{"kind": null}]}]}"#,
        );
        let m = meta(&graph);
        let found = violations(&m, &["bridge-core"], &["tokio"]).unwrap();
        assert_eq!(
            found,
            vec![vec![
                "bridge-core",
                "bridge-protocol",
                "serde",
                "helper",
                "tokio"
            ]]
        );
    }

    #[test]
    fn unknown_root_is_an_error() {
        let m = meta(GRAPH);
        assert!(violations(&m, &["bridge-missing"], &["tokio"]).is_err());
    }
}
