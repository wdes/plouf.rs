//! Graph data types for the `wiring.json` output.

/// A code-graph node: a file, class, interface, trait, enum, function, method,
/// or Vue component. `start`/`end` are byte offsets into the parsed source
/// (the file itself, or -- for `.vue` -- its extracted `<script>`), so the query
/// layer can slice a symbol's signature/body without re-parsing.
#[derive(Clone)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub path: String,
    pub start: u32,
    pub end: u32,
}

/// An edge before target resolution. `contains` carries a resolved `target_id`;
/// every other relation carries a `name` (and, for member `calls`, a `recv_type`)
/// resolved against the whole-tree index later.
pub struct RawEdge {
    pub source: String,
    pub relation: &'static str,
    pub target_id: Option<String>,
    pub name: Option<String>,
    pub via_member: bool,
    pub recv_type: Option<String>,
}

/// A fully resolved edge (`source`/`target` are node ids or kept-raw names).
/// A flat struct rather than a `serde_json::Value` so the resolved set stays
/// cheap to hold and is streamed to the output one item at a time.
pub struct ResolvedEdge {
    pub source: String,
    pub target: String,
    pub relation: &'static str,
}

impl RawEdge {
    pub const fn contains(source: String, target_id: String) -> Self {
        Self { source, relation: "contains", target_id: Some(target_id), name: None, via_member: false, recv_type: None }
    }
    pub const fn named(source: String, relation: &'static str, name: String) -> Self {
        Self { source, relation, target_id: None, name: Some(name), via_member: false, recv_type: None }
    }
    pub const fn call(source: String, name: String, via_member: bool, recv_type: Option<String>) -> Self {
        Self { source, relation: "calls", target_id: None, name: Some(name), via_member, recv_type }
    }
}

/// Eloquent relation methods: a `$this->belongsTo(Related::class)` call yields a
/// model-class -> related-class edge, resolved by unique class name (like
/// heritage). The method name doubles as the `&'static` edge relation so the
/// graph records which kind of relation it is.
pub const RELATION_KINDS: &[&str] = &[
    "belongsTo", "hasMany", "hasOne", "belongsToMany", "morphTo", "morphMany", "morphOne",
    "morphToMany", "morphedByMany", "hasManyThrough", "hasOneThrough",
];

/// The `&'static` relation label for an Eloquent relation method, if it is one.
pub fn relation_kind(method: &str) -> Option<&'static str> {
    RELATION_KINDS.iter().copied().find(|&r| r == method)
}

/// Edges whose target is a class node resolved by unique name: heritage plus the
/// Eloquent relation kinds. `table`/`migrates` (target = a `table:` node) and the
/// others resolve differently.
pub fn resolves_to_class(relation: &str) -> bool {
    matches!(relation, "extends" | "implements") || RELATION_KINDS.contains(&relation)
}
