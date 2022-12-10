use std::fmt::Debug;

use serde::{Deserialize, Serialize};

use crate::math::transform::Transform;

#[cfg(feature = "opengl")]
use crate::renderers::opengl::OpenglRenderer;

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[repr(transparent)]
pub struct NodeUuid(pub(crate) u32);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeState {
    pub uuid: NodeUuid,
    pub name: String,
    pub enabled: bool,
    pub zsort: f32,
    pub transform: Transform,
    #[serde(rename = "lockToRoot")]
    pub lock_to_root: bool,
}

// TODO: make a derive macro for this
#[typetag::serde(tag = "type")]
pub trait Node: Debug {
    fn get_node_state(&self) -> &NodeState;
    fn get_node_state_mut(&mut self) -> &mut NodeState;

    #[cfg(feature = "opengl")]
    fn render(&self, _renderer: &OpenglRenderer) {}
}

#[typetag::serde(name = "Node")]
impl Node for NodeState {
    fn get_node_state(&self) -> &NodeState {
        self
    }

    fn get_node_state_mut(&mut self) -> &mut NodeState {
        self
    }
}
