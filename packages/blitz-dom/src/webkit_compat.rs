//! acrender: the `-webkit-` line clamping that Stylo's servo build does not parse.
//!
//! `-webkit-line-clamp` and `display: -webkit-box` are gecko-only in Stylo, so they are dropped
//! as invalid and the box is neither clamped nor block-level. CSS is rewritten before Stylo
//! parses it (stylesheets, `@import`s, style attributes, CSSOM `cssText`):
//! `-webkit-line-clamp` becomes the custom property `--acrender-line-clamp` (read by inline layout,
//! see [`line_clamp`]), `-webkit-box` becomes `flow-root` and `-webkit-inline-box` becomes
//! `inline-block` (what a vertical `-webkit-box` with only inline content lays out like).
use std::borrow::Cow;

use style::Atom;

use crate::{NodeId, NodeTree};

const MAP: [(&str, &str); 3] = [
    ("-webkit-line-clamp", "--acrender-line-clamp"),
    ("-webkit-inline-box", "inline-block"),
    ("-webkit-box", "flow-root"),
];

/// Rewrites the `-webkit-` identifiers in [`MAP`] (whole identifiers only: `-webkit-box-orient`
/// and `-webkit-box-shadow` are left alone).
pub(crate) fn webkit_compat(css: &str) -> Cow<'_, str> {
    if !css.contains("-webkit-") {
        return Cow::Borrowed(css);
    }
    let mut out = String::with_capacity(css.len() + 8);
    let mut rest = css;
    while let Some(i) = rest.find("-webkit-") {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let found = MAP.iter().find(|(from, _)| {
            tail.starts_with(from)
                && !tail[from.len()..]
                    .starts_with(|c: char| c == '-' || c == '_' || c.is_alphanumeric())
        });
        let (from, to) = found.copied().unwrap_or(("-webkit-", "-webkit-"));
        out.push_str(to);
        rest = &tail[from.len()..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The `-webkit-line-clamp` line count the node itself declares (not one inherited from its
/// parent: the custom property standing in for it inherits).
pub(crate) fn line_clamp(nodes: &NodeTree, id: NodeId) -> Option<usize> {
    let name = Atom::from("acrender-line-clamp");
    let value = |id: NodeId| {
        let style = nodes.get(id)?.primary_styles()?;
        style
            .custom_properties()
            .get_for_cssom(&name)?
            .as_universal()
            .cloned()
    };
    let own = value(id)?;
    let inherited = nodes[id].parent.and_then(value);
    if inherited.is_some_and(|p| style::servo_arc::Arc::ptr_eq(&p, &own)) {
        return None;
    }
    own.css_text()
        .trim()
        .parse()
        .ok()
        .filter(|&n: &usize| n > 0)
}
