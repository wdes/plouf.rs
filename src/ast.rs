//! Small, pure helpers over Mago's PHP AST (names, type hints, call operands).

use mago_syntax::cst::cst::{
    ClassLikeMemberSelector, Expression, Hint, Identifier, Variable,
};

/// A byte-slice AST value as an owned `String` (lossy on invalid UTF-8).
pub fn bytes(v: &[u8]) -> String {
    String::from_utf8_lossy(v).into_owned()
}

/// The trailing segment of a `\`-qualified name (`App\Models\User` -> `User`).
pub fn dequalify(name: &str) -> String {
    name.rsplit('\\').next().unwrap_or(name).to_string()
}

/// The full (leading-`\`-trimmed) text of an identifier.
pub fn ident_full(id: &Identifier) -> String {
    let raw = match id {
        Identifier::Local(l) => bytes(l.value),
        Identifier::Qualified(q) => bytes(q.value),
        Identifier::FullyQualified(f) => bytes(f.value),
    };
    raw.trim_start_matches('\\').to_string()
}

/// The bare (de-qualified) name of an identifier.
pub fn ident_name(id: &Identifier) -> String {
    dequalify(&ident_full(id))
}

/// The single class named by a type hint, if any (unwraps `?T`/`(T)`; `None`
/// for unions, primitives, `array`, `void`, ...).
pub fn hint_class(h: &Hint) -> Option<String> {
    match h {
        Hint::Identifier(id) => Some(ident_name(id)),
        Hint::Nullable(n) => hint_class(n.hint),
        Hint::Parenthesized(p) => hint_class(p.hint),
        _ => None,
    }
}

/// The member name of a `->m`/`::m` selector, when it's a plain identifier.
pub fn selector_name(sel: &ClassLikeMemberSelector) -> Option<String> {
    match sel {
        ClassLikeMemberSelector::Identifier(id) => Some(bytes(id.value)),
        _ => None,
    }
}

/// The callee name of a `foo()` call, when it's a plain identifier.
pub fn callee_name(expr: &Expression) -> Option<String> {
    match expr {
        Expression::Identifier(id) => Some(ident_name(id)),
        _ => None,
    }
}

/// The `$var` text of a direct-variable expression (e.g. the `->` receiver).
pub fn var_name(expr: &Expression) -> Option<String> {
    match expr {
        Expression::Variable(Variable::Direct(dv)) => Some(bytes(dv.name)),
        _ => None,
    }
}
