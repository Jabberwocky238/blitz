//! acrender: `:has()` invalidation. Stylo's snapshot invalidation only looks at an element and the elements its
//! selectors reach rightwards; a `:has()` anchor sits to the left (an ancestor or an earlier sibling) of the
//! element that changed, so a state change, an insertion or a removal has to be told to Stylo's
//! `RelativeSelectorInvalidator`, which walks there. Gecko does the same from its glue
//! (`Servo_StyleSet_MaybeInvalidateRelativeSelector{StateDependency,ForInsertion,ForRemoval}`).

use std::marker::PhantomData;

use blitz_traits::node_id::NodeId;
use selectors::Element as SelectorsElement;
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
        let mut used = false;
        self.stylist.for_each_cascade_data_with_scope(element, |data, _| used |= data.relative_invalidation_map_attributes().used);
        if !used {
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
        let inherited = inherited_search_direction(TElement::traversal_parent(&element), SelectorsElement::prev_sibling_element(&element));
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
        let siblings = SiblingTraversalMap::new(element, SelectorsElement::prev_sibling_element(&element), SelectorsElement::next_sibling_element(&element));
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
