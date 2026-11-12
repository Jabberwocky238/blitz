//! acrender: the events of CSS animations and transitions (css-animations-1 § 4.2, css-transitions-1 § 6.1), queued while
//! the style pass advances them and taken by the page. Elapsed times follow Servo's `script/animations.rs`.

use std::collections::VecDeque;

use blitz_traits::node_id::NodeId;
use cssparser::ToCss;
use style::animation::{Animation, AnimationSetKey, KeyframesIterationState, Transition};
use style::selector_parser::PseudoElement;

const LIMIT: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnimationEventKind {
    AnimationStart,
    AnimationIteration,
    AnimationEnd,
    AnimationCancel,
    TransitionRun,
    TransitionStart,
    TransitionEnd,
    TransitionCancel,
}

impl AnimationEventKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::AnimationStart => "animationstart",
            Self::AnimationIteration => "animationiteration",
            Self::AnimationEnd => "animationend",
            Self::AnimationCancel => "animationcancel",
            Self::TransitionRun => "transitionrun",
            Self::TransitionStart => "transitionstart",
            Self::TransitionEnd => "transitionend",
            Self::TransitionCancel => "transitioncancel",
        }
    }

    pub fn is_transition(self) -> bool {
        matches!(self, Self::TransitionRun | Self::TransitionStart | Self::TransitionEnd | Self::TransitionCancel)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnimationEvent {
    pub kind: AnimationEventKind,
    pub node: NodeId,
    pub pseudo: String,
    pub name: String,
    pub elapsed: f64,
}

#[derive(Default)]
pub(crate) struct AnimationEvents {
    pub(crate) wanted: bool,
    queue: VecDeque<AnimationEvent>,
}

impl AnimationEvents {
    pub(crate) fn take(&mut self) -> Vec<AnimationEvent> {
        self.wanted = true;
        self.queue.drain(..).collect()
    }

    fn push(&mut self, key: &AnimationSetKey, kind: AnimationEventKind, name: String, elapsed: f64) {
        if !self.wanted {
            return;
        }
        if self.queue.len() == LIMIT {
            self.queue.pop_front();
        }
        self.queue.push_back(AnimationEvent {
            kind,
            node: NodeId::from_u64(key.node.id() as u64),
            pseudo: key.pseudo_element.as_ref().map(PseudoElement::to_css_string).unwrap_or_default(),
            name,
            elapsed: (elapsed.abs() * 1e6).round() / 1e6,
        });
    }

    pub(crate) fn animation(&mut self, key: &AnimationSetKey, animation: &Animation, kind: AnimationEventKind, now: f64) {
        if !self.wanted {
            return;
        }
        let (iteration, active) = match animation.iteration_state {
            KeyframesIterationState::Finite(current, max) => (current, max * animation.duration),
            KeyframesIterationState::Infinite(current) => (current, f64::MAX),
        };
        let elapsed = match kind {
            AnimationEventKind::AnimationStart => (-animation.delay).max(0.).min(active),
            AnimationEventKind::AnimationIteration => iteration * animation.duration,
            AnimationEventKind::AnimationEnd => iteration * animation.duration + animation.current_iteration_duration(),
            _ => iteration * animation.duration + (now - animation.started_at).max(0.),
        };
        self.push(key, kind, animation.name.to_string(), elapsed);
    }

    pub(crate) fn transition(&mut self, key: &AnimationSetKey, transition: &Transition, kind: AnimationEventKind, now: f64) {
        if !self.wanted {
            return;
        }
        let duration = transition.property_animation.duration;
        let elapsed = match kind {
            AnimationEventKind::TransitionEnd => duration,
            AnimationEventKind::TransitionCancel => (now - transition.start_time).max(0.),
            _ => duration.min((-transition.delay).max(0.)),
        };
        let name = transition.property_animation.property_id().name().into_owned();
        self.push(key, kind, name, elapsed);
    }
}

pub(crate) fn has_ended(animation: &Animation, now: f64) -> bool {
    let last = match animation.iteration_state {
        KeyframesIterationState::Finite(current, max) => max - current <= 1.,
        KeyframesIterationState::Infinite(_) => false,
    };
    last && now - animation.started_at >= animation.current_iteration_duration()
}
