//! bbscript format: a Gherkin-like end-to-end test DSL. A `*.bbscript` file is a
//! `Feature:` with a `Background:` and one or more `Scenario:` blocks of indented
//! steps (`open /login`, `fill ...`, `click ...`, `seeurl ends /clients`, ...).
//!
//! We emit a `file` node, a `scenario` node per Scenario, and a `visits` edge to
//! a `route:<path>` node for every step that references a route -- so the graph
//! knows which e2e tests exercise which routes. (`route:<path>` mirrors the
//! `table:<name>` join node: a Vue-router pass can later link a route to its page
//! component, connecting a scenario to the source it drives.)

use std::collections::HashSet;

use crate::format::Format;
use crate::model::{Node, RawEdge};

/// The bbscript format: routes every `*.bbscript`.
pub struct BbScript;

impl Format for BbScript {
    fn matches(&self, _base: &str, ext: &str) -> bool {
        ext == "bbscript"
    }

    fn extract(&self, rel: &str, base: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>) {
        extract(rel, base, code)
    }
}

/// Parse a bbscript file into a `file` node, per-`Scenario:` `scenario` nodes,
/// and `visits` edges to the routes each references.
pub fn extract(rel: &str, base: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>) {
    let mut nodes = vec![Node { id: rel.to_string(), name: base.to_string(), kind: "file", path: rel.to_string(), start: 0, end: 0 }];
    let mut edges = Vec::new();
    let mut minted: HashSet<String> = HashSet::new();
    minted.insert(rel.to_string());
    let mut routes: HashSet<String> = HashSet::new();

    // Steps before the first `Scenario:` (the `Background:` / `Feature:`
    // preamble) attribute to the file itself.
    let mut scope = rel.to_string();

    for raw in code.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix("Scenario:") {
            let name = name.trim();
            let id = mint(&mut minted, format!("{rel}#{name}"));
            edges.push(RawEdge::contains(rel.to_string(), id.clone()));
            nodes.push(Node { id: id.clone(), name: name.to_string(), kind: "scenario", path: rel.to_string(), start: 0, end: 0 });
            scope = id;
        } else if line.starts_with("Feature:") || line.starts_with("Background:") {
            scope = rel.to_string();
        } else if let Some(route) = route_of(line) {
            edges.push(RawEdge::named(scope.clone(), "visits", route.clone()));
            if routes.insert(route.clone()) {
                nodes.push(Node { id: format!("route:{route}"), name: route, kind: "route", path: rel.to_string(), start: 0, end: 0 });
            }
        }
    }
    (nodes, edges)
}

/// Collision-proof id (`~2`/`~3`/...), matching the walkers.
fn mint(minted: &mut HashSet<String>, base: String) -> String {
    if minted.insert(base.clone()) {
        return base;
    }
    let mut k = 2u32;
    loop {
        let cand = format!("{base}~{k}");
        if minted.insert(cand.clone()) {
            return cand;
        }
        k += 1;
    }
}

/// The route a step references: the first whitespace token starting with `/`,
/// with any `?query` / `#fragment` trimmed. `None` for steps with no route.
fn route_of(step: &str) -> Option<String> {
    let token = step.split_whitespace().find(|t| t.starts_with('/'))?;
    let path = token.split(['?', '#']).next().unwrap_or(token);
    (!path.is_empty()).then(|| path.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::extract;

    #[test]
    fn emits_feature_scenarios_and_route_visits() {
        let code = "Feature: logout\n\n# a comment\nBackground:\n  open /login\n  fill [name=\"email\"] a@b.c\nScenario: redirects when logged out\n  open /clients\n  seeurl has /login?redirect=\nScenario: can log back in\n  reload\n  seeurl ends /clients\n";
        let (nodes, edges) = extract("tests/e2e/logout.bbscript", "logout.bbscript", code);

        // one file + two scenarios + the distinct route nodes
        assert!(nodes.iter().any(|n| n.kind == "file"));
        let scenarios: Vec<&str> = nodes.iter().filter(|n| n.kind == "scenario").map(|n| n.name.as_str()).collect();
        assert!(scenarios.contains(&"redirects when logged out") && scenarios.contains(&"can log back in"));
        let route_nodes: Vec<&str> = nodes.iter().filter(|n| n.kind == "route").map(|n| n.name.as_str()).collect();
        assert!(route_nodes.contains(&"/login") && route_nodes.contains(&"/clients"));

        // Background `open /login` attributes to the file; scenario steps to the scenario.
        assert!(edges.iter().any(|e| e.relation == "visits" && e.source == "tests/e2e/logout.bbscript" && e.name.as_deref() == Some("/login")));
        assert!(edges
            .iter()
            .any(|e| e.relation == "visits" && e.source.ends_with("#redirects when logged out") && e.name.as_deref() == Some("/clients")));
        // The query-string is trimmed to the route path.
        assert!(edges.iter().any(|e| e.relation == "visits" && e.name.as_deref() == Some("/login")));
        // A non-route step (fill ...) yields no visit.
        assert!(!edges.iter().any(|e| e.relation == "visits" && e.name.as_deref() == Some("a@b.c")));
    }

    #[test]
    fn duplicate_scenario_names_get_distinct_ids() {
        let (nodes, _) = extract("x.bbscript", "x.bbscript", "Scenario: same\n  open /a\nScenario: same\n  open /b\n");
        let ids: Vec<&str> = nodes.iter().filter(|n| n.kind == "scenario").map(|n| n.id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]); // the second `same` is minted with a ~2 suffix
    }

    #[test]
    fn steps_without_a_route_are_ignored() {
        let code = "Scenario: nothing navigational\n  storage remove mounch.user\n  see role:heading[name=\"Clients\"]\n";
        let (nodes, edges) = extract("x.bbscript", "x.bbscript", code);
        assert!(!edges.iter().any(|e| e.relation == "visits"));
        assert!(!nodes.iter().any(|n| n.kind == "route"));
    }
}
