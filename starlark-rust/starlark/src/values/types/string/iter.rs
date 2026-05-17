/*
 * Copyright 2018 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Implementation of iterators for string type.

use allocative::Allocative;
use derive_more::Display;
use starlark_derive::Freeze;
use starlark_derive::NoSerialize;
use starlark_derive::Trace;
use starlark_derive::starlark_value;

use crate as starlark;
use crate::any::ProvidesStaticType;
use crate::coerce::Coerce;
use crate::values::Heap;
use crate::values::StarlarkValue;
use crate::values::StringValue;
use crate::values::StringValueLike;
use crate::values::Value;
use crate::values::ValueLike;
use crate::values::ValueOfUnchecked;
use crate::values::index::apply_slice;
use crate::values::index::convert_index;
use crate::values::typing::iter::StarlarkIter;

/// An opaque iterator over a string, produced by elems/codepoints
#[derive(
    Debug,
    Trace,
    Coerce,
    Display,
    Freeze,
    NoSerialize,
    ProvidesStaticType,
    Allocative
)]
#[display("iterator")]
#[repr(C)]
struct StringIterableGen<'v, V: ValueLike<'v>> {
    string: V::String,
    produce_char: bool, // if not char, then int
}

impl<'v, V: ValueLike<'v>> StringIterableGen<'v, V> {
    fn collect_values(&self, heap: Heap<'v>) -> Vec<Value<'v>> {
        if self.produce_char {
            self.string
                .as_str()
                .chars()
                .map(|c| heap.alloc(c))
                .collect()
        } else {
            self.string
                .as_str()
                .chars()
                .map(|c| heap.alloc(u32::from(c)))
                .collect()
        }
    }

    fn alloc_at(&self, index: usize, heap: Heap<'v>) -> Value<'v> {
        let c = self.string.as_str().chars().nth(index).unwrap();
        if self.produce_char {
            heap.alloc(c)
        } else {
            heap.alloc(u32::from(c))
        }
    }
}

pub(crate) fn iterate_chars<'v>(
    string: StringValue<'v>,
    heap: Heap<'v>,
) -> ValueOfUnchecked<'v, StarlarkIter<String>> {
    ValueOfUnchecked::new(heap.alloc_complex(StringIterableGen::<'v, Value<'v>> {
        string,
        produce_char: true,
    }))
}

pub(crate) fn iterate_codepoints<'v>(
    string: StringValue<'v>,
    heap: Heap<'v>,
) -> ValueOfUnchecked<'v, StarlarkIter<String>> {
    ValueOfUnchecked::new(heap.alloc_complex(StringIterableGen::<'v, Value<'v>> {
        string,
        produce_char: false,
    }))
}

#[starlark_value(type = "iterator")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for StringIterableGen<'v, V>
where
    Self: ProvidesStaticType<'v>,
{
    fn at(&self, index: Value<'v>, heap: Heap<'v>) -> crate::Result<Value<'v>> {
        let index = convert_index(index, self.length()?)?;
        Ok(self.alloc_at(index as usize, heap))
    }

    fn length(&self) -> crate::Result<i32> {
        Ok(self.string.as_str().chars().count() as i32)
    }

    fn slice(
        &self,
        start: Option<Value<'v>>,
        stop: Option<Value<'v>>,
        stride: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> crate::Result<Value<'v>> {
        let values = self.collect_values(heap);
        Ok(heap.alloc_list(&apply_slice(&values, start, stop, stride)?))
    }

    unsafe fn iterate(&self, _me: Value<'v>, heap: Heap<'v>) -> crate::Result<Value<'v>> {
        // Lazy implementation: we allocate a tuple and then iterate over it.
        Ok(heap.alloc_tuple_iter(self.collect_values(heap)))
    }
}
