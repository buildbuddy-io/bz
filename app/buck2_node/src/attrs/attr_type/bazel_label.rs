use allocative::Allocative;
use pagable::Pagable;

use crate::attrs::attr_type::AttrType;

#[derive(Debug, Hash, Pagable, Eq, PartialEq, Allocative)]
pub struct BazelLabelAttrType {
    pub dep: AttrType,
    pub source: AttrType,
}

impl BazelLabelAttrType {
    pub fn new(dep: AttrType, source: AttrType) -> Self {
        Self { dep, source }
    }
}
