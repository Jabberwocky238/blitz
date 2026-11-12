//! acrender: `:has()` invalidation. Stylo's snapshot invalidation only looks at an element and the elements its
//! selectors reach rightwards; a `:has()` anchor sits to the left (an ancestor or an earlier sibling) of the
//! element that changed, so a state change, an insertion or a removal has to be told to Stylo's
//! `RelativeSelectorInvalidator`, which walks there. Gecko does the same from its glue
//! (`Servo_StyleSet_MaybeInvalidateRelativeSelector{StateDependency,ForInsertion,ForRemoval}`).

use std::marker::PhantomData;

use blitz_traits::node_id::NodeId;
use selectors::matching::ElementSelectorFlags;
use style::dom::TElement;
use style::invalidation::element::invalidator::{InvalidationResult, SiblingTraversalMap};
use style::invalidation::element::relative_selector::{DomMutationOperation, RelativeSelectorInvalidator};
use style::thread_state::{self, ThreadState};
use markup5ever::local_name;
use style::Atom;
use style_dom::ElementState;

use crate::BaseDocument;
use crate::node::Node;

fn invalidated(element: &Node, result: &InvalidationResult) {
    if result.has_invalidated_siblings() {
        if let Some(parent) = TElement::traversal_parent(&element) {
            parent.set_dirty_descendants();
            parent.mark_ancestors_dirty();
        }
    } else if result.has_invalidated_descendants() {
        element.set_dirty_descendants();
        element.mark_ancestors_dirty();
    } else if result.has_invalidated_self() {
        element.mark_ancestors_dirty();
    }
}

fn inherited_search_direction(parent: Option<&Node>, prev_sibling: Option<&Node>) -> ElementSelectorFlags {
    let mut inherited = ElementSelectorFlags::empty();
    if let Some(parent) = parent {
        inherited |=
            parent.relative_selector_search_direction() & ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR;
    }
    if let Some(sibling) = prev_sibling {
        inherited |= sibling.relative_selector_search_direction()
            & ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING;
    }
    inherited
}

impl BaseDocument {
    fn has_anchor_candidate(&self, node_id: NodeId) -> Option<&Node> {
        let node = self.nodes.get(node_id)?;
        (node.is_element() && node.flags.is_in_document() && node.stylo_element_data().is_some()).then_some(node)
    }

    /// Whether any style data that applies to `element` has a `:has()` selector: when none does, no change of
    /// `element` can invalidate a `:has()` anchor
    fn uses_has(&self, element: &Node) -> bool {
        let mut used = false;
        self.stylist.for_each_cascade_data_with_scope(element, |data, _| used |= data.relative_invalidation_map_attributes().used);
        used
    }

    /// The element siblings of `node` before and after it. Searched from the end of the parent's children, so the
    /// child just appended (the common insertion) is found at once; `Node::backward` / `forward` search from the start,
    /// which made each append a scan of all earlier children (#206)
    fn element_siblings<'a>(&'a self, node: &Node) -> (Option<&'a Node>, Option<&'a Node>) {
        let Some(children) = node.parent.and_then(|parent| self.nodes.get(parent)).map(|parent| parent.children.as_slice()) else {
            return (None, None);
        };
        let Some(at) = children.iter().rposition(|id| *id == node.id) else {
            return (None, None);
        };
        let element = |id: &NodeId| self.nodes.get(*id).filter(|n| n.is_element());
        (children[..at].iter().rev().find_map(element), children[at + 1..].iter().find_map(element))
    }

    fn invalidator<'a>(&self, element: &'a Node, siblings: SiblingTraversalMap<&'a Node>) -> RelativeSelectorInvalidator<'a, 'a, &'a Node> {
        RelativeSelectorInvalidator {
            element,
            quirks_mode: self.stylist.quirks_mode(),
            snapshot_table: None,
            invalidated,
            sibling_traversal_map: siblings,
            _marker: PhantomData,
        }
    }

    /// The `:has()` anchors that depend on `state` of `node_id`, about to change
    pub(crate) fn invalidate_has_for_state(&self, node_id: NodeId, state: ElementState) {
        let Some(element) = self.has_anchor_candidate(node_id) else {
            return;
        };
        thread_state::enter(ThreadState::LAYOUT);
        self.invalidator(element, SiblingTraversalMap::default()).invalidate_relative_selectors_for_this(
            &self.stylist,
            |element, scope, data, quirks_mode, collector| {
                data.relative_selector_invalidation_map().state_affecting_selectors.lookup(
                    *element,
                    quirks_mode,
                    None,
                    |dependency| {
                        if dependency.state.intersects(state) {
                            collector.add_dependency(&dependency.dep, *element, scope);
                        }
                        true
                    },
                );
            },
        );
        thread_state::exit(ThreadState::LAYOUT);
    }

    /// The `:has()` anchors that depend on attribute `name` of `node_id`, about to change to `value` (None: removed):
    /// the classes or ids it had or will have, or the attribute's name
    pub(crate) fn invalidate_has_for_attribute(&self, node_id: NodeId, name: &crate::QualName, value: Option<&str>) {
        let Some(element) = self.has_anchor_candidate(node_id) else {
            return;
        };
        if !self.uses_has(element) {
            return;
        }
        let old = element.attr(name.local.clone()).map(str::to_owned);
        let words = |text: Option<&str>| {
            text.into_iter().flat_map(str::split_ascii_whitespace).map(Atom::from).collect::<Vec<_>>()
        };
        let (class, id) = (name.local == local_name!("class"), name.local == local_name!("id"));
        let mut keys = words(old.as_deref());
        keys.extend(words(value));
        thread_state::enter(ThreadState::LAYOUT);
        self.invalidator(element, SiblingTraversalMap::default()).invalidate_relative_selectors_for_this(
            &self.stylist,
            |element, scope, data, quirks_mode, collector| {
                let map = data.relative_selector_invalidation_map();
                let lists = keys
                    .iter()
                    .filter_map(|key| match (class, id) {
                        (true, _) => map.class_to_selector.get(key, quirks_mode),
                        (_, true) => map.id_to_selector.get(key, quirks_mode),
                        _ => None,
                    })
                    .chain(map.other_attribute_affecting_selectors.get(&name.local));
                for dependency in lists.flatten() {
                    collector.add_dependency(dependency, *element, scope);
                }
            },
        );
        thread_state::exit(ThreadState::LAYOUT);
    }

    /// The `:has()` anchors `node_id`, just inserted, and its subtree may match now
    pub(crate) fn invalidate_has_for_insertion(&self, node_id: NodeId) {
        let Some(element) = self.has_anchor_candidate(node_id) else {
            return;
        };
        if !self.uses_has(element) {
            return;
        }
        let inherited = inherited_search_direction(TElement::traversal_parent(&element), self.element_siblings(element).0);
        if inherited.is_empty() {
            return;
        }
        thread_state::enter(ThreadState::LAYOUT);
        self.invalidator(element, SiblingTraversalMap::default()).invalidate_relative_selectors_for_dom_mutation(
            true,
            &self.stylist,
            inherited,
            DomMutationOperation::Insert,
        );
        thread_state::exit(ThreadState::LAYOUT);
    }

    /// The `:has()` anchors `node_id` and its subtree, about to be removed, matched through
    pub(crate) fn invalidate_has_for_removal(&self, node_id: NodeId) {
        let Some(element) = self.has_anchor_candidate(node_id) else {
            return;
        };
        if element.relative_selector_search_direction().is_empty() {
            return;
        }
        let (prev, next) = self.element_siblings(element);
        let siblings = SiblingTraversalMap::new(element, prev, next);
        thread_state::enter(ThreadState::LAYOUT);
        self.invalidator(element, siblings).invalidate_relative_selectors_for_dom_mutation(
            true,
            &self.stylist,
            ElementSelectorFlags::empty(),
            DomMutationOperation::Remove,
        );
        thread_state::exit(ThreadState::LAYOUT);
    }
}

#[cfg(test)]
mod tests {
    use blitz_traits::shell::{ColorScheme, Viewport};

    use crate::{BaseDocument, DocumentConfig, qual_name};

    fn document(css: &str) -> (BaseDocument, crate::NodeId) {
        let mut document =
            BaseDocument::new(DocumentConfig { viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)), ..Default::default() });
        let root = document.root_node().id;
        let html = {
            let mut mutator = document.mutate();
            let html = mutator.create_element(qual_name!("html", html), vec![]);
            let style = mutator.create_element(qual_name!("style", html), vec![]);
            let text = mutator.create_text_node(css);
            mutator.append_children(style, &[text]);
            mutator.append_children(html, &[style]);
            mutator.append_children(root, &[html]);
            html
        };
        document.resolve(0.0);
        (document, html)
    }

    fn div(document: &mut BaseDocument, parent: crate::NodeId, class: &str) -> crate::NodeId {
        let mut mutator = document.mutate();
        let id = mutator.create_element(qual_name!("div", html), vec![]);
        if !class.is_empty() {
            mutator.set_attribute(id, crate::QualName::new(None, markup5ever::ns!(), "class".into()), class);
        }
        mutator.append_children(parent, &[id]);
        id
    }

    fn text(document: &mut BaseDocument, parent: crate::NodeId) {
        let mut mutator = document.mutate();
        let id = mutator.create_text_node(" ");
        mutator.append_children(parent, &[id]);
    }

    fn width(document: &BaseDocument, id: crate::NodeId) -> f32 {
        document.get_node(id).unwrap().final_layout().size.width
    }

    #[test]
    fn element_siblings_skip_text_from_either_end() {
        let (mut document, html) = document("");
        let a = div(&mut document, html, "");
        text(&mut document, html);
        let b = div(&mut document, html, "");
        text(&mut document, html);
        let c = div(&mut document, html, "");
        let ids = |node: crate::NodeId| {
            let (prev, next) = document.element_siblings(document.get_node(node).unwrap());
            (prev.map(|n| n.id), next.map(|n| n.id))
        };
        assert_eq!(ids(b), (Some(a), Some(c)));
        assert_eq!(ids(c), (Some(b), None), "the last child, an append, is found from the end");
        assert_eq!(ids(html), (None, None), "no element siblings");
    }

    #[test]
    fn an_appended_child_after_text_still_reaches_has_anchors() {
        style_config::set_pref!("layout.css.has-selector.enabled", true);
        let (mut document, html) = document(".host:has(.hit) { width: 50px } .prev:has(+ .hit) { width: 60px }");
        let host = div(&mut document, html, "host");
        let prev = div(&mut document, host, "prev");
        text(&mut document, host);
        document.resolve(0.0);
        assert_eq!((width(&document, host), width(&document, prev)), (800.0, 800.0));
        let hit = div(&mut document, host, "hit");
        document.resolve(0.0);
        assert_eq!((width(&document, host), width(&document, prev)), (50.0, 60.0));
        document.mutate().remove_node(hit);
        document.resolve(0.0);
        assert_eq!((width(&document, host), width(&document, prev)), (800.0, 800.0));
    }

    #[test]
    fn without_has_an_insertion_is_left_alone() {
        let (mut document, html) = document(".x { width: 10px }");
        let host = div(&mut document, html, "");
        document.resolve(0.0);
        let kid = div(&mut document, host, "");
        let element = document.get_node(kid).unwrap();
        assert!(!document.uses_has(element));
    }
}
