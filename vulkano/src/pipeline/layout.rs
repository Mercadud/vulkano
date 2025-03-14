// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

//! The layout of descriptor sets and push constants used by a pipeline.
//!
//! # Overview
//!
//! The layout itself only *describes* the descriptors and push constants, and does not contain
//! their content itself. Instead, you can think of it as a `struct` definition that states which
//! members there are, what types they have, and in what order.
//! One could imagine a Rust definition somewhat like this:
//!
//! ```text
//! #[repr(C)]
//! struct MyPipelineLayout {
//!     push_constants: Pc,
//!     descriptor_set0: Ds0,
//!     descriptor_set1: Ds1,
//!     descriptor_set2: Ds2,
//!     descriptor_set3: Ds3,
//! }
//! ```
//!
//! Of course, a pipeline layout is created at runtime, unlike a Rust type.
//!
//! # Layout compatibility
//!
//! When binding descriptor sets or setting push constants, you must provide a pipeline layout.
//! This layout is used to decide where in memory Vulkan should write the new data. The
//! descriptor sets and push constants can later be read by dispatch or draw calls, but only if
//! the bound pipeline being used for the command has a layout that is *compatible* with the layout
//! that was used to bind the resources.
//!
//! *Compatible* means that the pipeline layout must be the same object, or a different layout in
//! which the push constant ranges and descriptor set layouts were be identically defined.
//! However, Vulkan allows for partial compatibility as well. In the `struct` analogy used above,
//! one could imagine that using a different definition would leave some members with the same
//! offset and size within the struct as in the old definition, while others are no longer
//! positioned correctly. For example, if a new, incompatible type were used for `Ds1`, then the
//! `descriptor_set1`, `descriptor_set2` and `descriptor_set3` members would no longer be correct,
//! but `descriptor_set0` and `push_constants` would remain accessible in the new layout.
//! Because of this behaviour, the following rules apply to compatibility between the layouts used
//! in subsequent descriptor set binding calls:
//!
//! - An incompatible definition of `Pc` invalidates all bound descriptor sets.
//! - An incompatible definition of `DsN` invalidates all bound descriptor sets *N* and higher.
//! - If *N* is the highest set being assigned in a bind command, and it and all lower sets
//!   have compatible definitions, including the push constants, then descriptor sets above *N*
//!   remain valid.
//!
//! [`AutoCommandBufferBuilder`](crate::command_buffer::auto::AutoCommandBufferBuilder) keeps
//! track of this state and will automatically remove descriptor sets that have been invalidated
//! by incompatible layouts in subsequent binding commands.
//!
//! # Creating pipeline layouts
//!
//! A pipeline layout is a Vulkan object type, represented in Vulkano with the `PipelineLayout`
//! type. Each pipeline that you create holds a pipeline layout object.

use crate::{
    descriptor_set::layout::{
        DescriptorRequirementsNotMet, DescriptorSetLayout, DescriptorSetLayoutBinding,
        DescriptorSetLayoutCreateFlags, DescriptorSetLayoutCreateInfo, DescriptorType,
    },
    device::{Device, DeviceOwned, Properties},
    macros::{impl_id_counter, vulkan_bitflags},
    shader::{
        DescriptorBindingRequirements, PipelineShaderStageCreateInfo, ShaderStage, ShaderStages,
    },
    RuntimeError, ValidationError, VulkanError, VulkanObject,
};
use ahash::HashMap;
use smallvec::SmallVec;
use std::{
    array,
    cmp::max,
    collections::hash_map::Entry,
    error::Error,
    fmt::{Display, Error as FmtError, Formatter, Write},
    mem::MaybeUninit,
    num::NonZeroU64,
    ptr,
    sync::Arc,
};

/// Describes the layout of descriptor sets and push constants that are made available to shaders.
#[derive(Debug)]
pub struct PipelineLayout {
    handle: ash::vk::PipelineLayout,
    device: Arc<Device>,
    id: NonZeroU64,

    flags: PipelineLayoutCreateFlags,
    set_layouts: Vec<Arc<DescriptorSetLayout>>,
    push_constant_ranges: Vec<PushConstantRange>,

    push_constant_ranges_disjoint: Vec<PushConstantRange>,
}

impl PipelineLayout {
    /// Creates a new `PipelineLayout`.
    pub fn new(
        device: Arc<Device>,
        create_info: PipelineLayoutCreateInfo,
    ) -> Result<Arc<PipelineLayout>, VulkanError> {
        Self::validate_new(&device, &create_info)?;

        unsafe { Ok(Self::new_unchecked(device, create_info)?) }
    }

    fn validate_new(
        device: &Device,
        create_info: &PipelineLayoutCreateInfo,
    ) -> Result<(), ValidationError> {
        // VUID-vkCreatePipelineLayout-pCreateInfo-parameter
        create_info
            .validate(device)
            .map_err(|err| err.add_context("create_info"))?;

        Ok(())
    }

    #[cfg_attr(not(feature = "document_unchecked"), doc(hidden))]
    pub unsafe fn new_unchecked(
        device: Arc<Device>,
        create_info: PipelineLayoutCreateInfo,
    ) -> Result<Arc<PipelineLayout>, RuntimeError> {
        let &PipelineLayoutCreateInfo {
            flags,
            ref set_layouts,
            ref push_constant_ranges,
            _ne: _,
        } = &create_info;

        let set_layouts_vk: SmallVec<[_; 4]> = set_layouts.iter().map(|l| l.handle()).collect();
        let push_constant_ranges_vk: SmallVec<[_; 4]> = push_constant_ranges
            .iter()
            .map(|range| ash::vk::PushConstantRange {
                stage_flags: range.stages.into(),
                offset: range.offset,
                size: range.size,
            })
            .collect();

        let create_info_vk = ash::vk::PipelineLayoutCreateInfo {
            flags: flags.into(),
            set_layout_count: set_layouts_vk.len() as u32,
            p_set_layouts: set_layouts_vk.as_ptr(),
            push_constant_range_count: push_constant_ranges_vk.len() as u32,
            p_push_constant_ranges: push_constant_ranges_vk.as_ptr(),
            ..Default::default()
        };

        let handle = {
            let fns = device.fns();
            let mut output = MaybeUninit::uninit();
            (fns.v1_0.create_pipeline_layout)(
                device.handle(),
                &create_info_vk,
                ptr::null(),
                output.as_mut_ptr(),
            )
            .result()
            .map_err(RuntimeError::from)?;
            output.assume_init()
        };

        Ok(Self::from_handle(device, handle, create_info))
    }

    /// Creates a new `PipelineLayout` from a raw object handle.
    ///
    /// # Safety
    ///
    /// - `handle` must be a valid Vulkan object handle created from `device`.
    /// - `create_info` must match the info used to create the object.
    #[inline]
    pub unsafe fn from_handle(
        device: Arc<Device>,
        handle: ash::vk::PipelineLayout,
        create_info: PipelineLayoutCreateInfo,
    ) -> Arc<PipelineLayout> {
        let PipelineLayoutCreateInfo {
            flags,
            set_layouts,
            mut push_constant_ranges,
            _ne: _,
        } = create_info;

        // Sort the ranges for the purpose of comparing for equality.
        // The stage mask is guaranteed to be unique, so it's a suitable sorting key.
        push_constant_ranges.sort_unstable_by_key(|range| {
            (
                range.offset,
                range.size,
                ash::vk::ShaderStageFlags::from(range.stages),
            )
        });

        let mut push_constant_ranges_disjoint: Vec<PushConstantRange> =
            Vec::with_capacity(push_constant_ranges.len());

        if !push_constant_ranges.is_empty() {
            let mut min_offset = push_constant_ranges[0].offset;
            loop {
                let mut max_offset = u32::MAX;
                let mut stages = ShaderStages::empty();

                for range in push_constant_ranges.iter() {
                    // new start (begin next time from it)
                    if range.offset > min_offset {
                        max_offset = max_offset.min(range.offset);
                        break;
                    } else if range.offset + range.size > min_offset {
                        // inside the range, include the stage
                        // use the minimum of the end of all ranges that are overlapping
                        max_offset = max_offset.min(range.offset + range.size);
                        stages |= range.stages;
                    }
                }
                // finished all stages
                if stages.is_empty() {
                    break;
                }

                push_constant_ranges_disjoint.push(PushConstantRange {
                    stages,
                    offset: min_offset,
                    size: max_offset - min_offset,
                });
                // prepare for next range
                min_offset = max_offset;
            }
        }

        Arc::new(PipelineLayout {
            handle,
            device,
            id: Self::next_id(),
            flags,
            set_layouts,
            push_constant_ranges,
            push_constant_ranges_disjoint,
        })
    }

    /// Returns the flags that the pipeline layout was created with.
    #[inline]
    pub fn flags(&self) -> PipelineLayoutCreateFlags {
        self.flags
    }

    /// Returns the descriptor set layouts this pipeline layout was created from.
    #[inline]
    pub fn set_layouts(&self) -> &[Arc<DescriptorSetLayout>] {
        &self.set_layouts
    }

    /// Returns a slice containing the push constant ranges this pipeline layout was created from.
    ///
    /// The ranges are guaranteed to be sorted deterministically by offset, size, then stages.
    /// This means that two slices containing the same elements will always have the same order.
    #[inline]
    pub fn push_constant_ranges(&self) -> &[PushConstantRange] {
        &self.push_constant_ranges
    }

    /// Returns a slice containing the push constant ranges in with all disjoint stages.
    ///
    /// For example, if we have these `push_constant_ranges`:
    /// - `offset=0, size=4, stages=vertex`
    /// - `offset=0, size=12, stages=fragment`
    ///
    /// The returned value will be:
    /// - `offset=0, size=4, stages=vertex|fragment`
    /// - `offset=4, size=8, stages=fragment`
    ///
    /// The ranges are guaranteed to be sorted deterministically by offset, and
    /// guaranteed to be disjoint, meaning that there is no overlap between the ranges.
    #[inline]
    pub(crate) fn push_constant_ranges_disjoint(&self) -> &[PushConstantRange] {
        &self.push_constant_ranges_disjoint
    }

    /// Returns whether `self` is compatible with `other` for the given number of sets.
    #[inline]
    pub fn is_compatible_with(&self, other: &PipelineLayout, num_sets: u32) -> bool {
        let num_sets = num_sets as usize;
        assert!(num_sets >= self.set_layouts.len());

        if self == other {
            return true;
        }

        if self.push_constant_ranges != other.push_constant_ranges {
            return false;
        }

        let other_sets = match other.set_layouts.get(0..num_sets) {
            Some(x) => x,
            None => return false,
        };

        self.set_layouts
            .iter()
            .zip(other_sets)
            .all(|(self_set_layout, other_set_layout)| {
                self_set_layout.is_compatible_with(other_set_layout)
            })
    }

    /// Makes sure that `self` is a superset of the provided descriptor set layouts and push
    /// constant ranges. Returns an `Err` if this is not the case.
    pub fn ensure_compatible_with_shader<'a>(
        &self,
        descriptor_requirements: impl IntoIterator<
            Item = ((u32, u32), &'a DescriptorBindingRequirements),
        >,
        push_constant_range: Option<&PushConstantRange>,
    ) -> Result<(), PipelineLayoutSupersetError> {
        for ((set_num, binding_num), reqs) in descriptor_requirements.into_iter() {
            let layout_binding = self
                .set_layouts
                .get(set_num as usize)
                .and_then(|set_layout| set_layout.bindings().get(&binding_num));

            let layout_binding = match layout_binding {
                Some(x) => x,
                None => {
                    return Err(PipelineLayoutSupersetError::DescriptorMissing {
                        set_num,
                        binding_num,
                    })
                }
            };

            if let Err(error) = layout_binding.ensure_compatible_with_shader(reqs) {
                return Err(PipelineLayoutSupersetError::DescriptorRequirementsNotMet {
                    set_num,
                    binding_num,
                    error,
                });
            }
        }

        // FIXME: check push constants
        if let Some(range) = push_constant_range {
            for own_range in self.push_constant_ranges.iter() {
                if range.stages.intersects(own_range.stages) &&       // check if it shares any stages
                    (range.offset < own_range.offset || // our range must start before and end after the given range
                        own_range.offset + own_range.size < range.offset + range.size)
                {
                    return Err(PipelineLayoutSupersetError::PushConstantRange {
                        first_range: *own_range,
                        second_range: *range,
                    });
                }
            }
        }

        Ok(())
    }
}

impl Drop for PipelineLayout {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            let fns = self.device.fns();
            (fns.v1_0.destroy_pipeline_layout)(self.device.handle(), self.handle, ptr::null());
        }
    }
}

unsafe impl VulkanObject for PipelineLayout {
    type Handle = ash::vk::PipelineLayout;

    #[inline]
    fn handle(&self) -> Self::Handle {
        self.handle
    }
}

unsafe impl DeviceOwned for PipelineLayout {
    #[inline]
    fn device(&self) -> &Arc<Device> {
        &self.device
    }
}

impl_id_counter!(PipelineLayout);

/// Parameters to create a new `PipelineLayout`.
#[derive(Clone, Debug)]
pub struct PipelineLayoutCreateInfo {
    /// Specifies how to create the pipeline layout.
    pub flags: PipelineLayoutCreateFlags,

    /// The descriptor set layouts that should be part of the pipeline layout.
    ///
    /// They are provided in order of set number.
    ///
    /// The default value is empty.
    pub set_layouts: Vec<Arc<DescriptorSetLayout>>,

    /// The ranges of push constants that the pipeline will access.
    ///
    /// A shader stage can only appear in one element of the list, but it is possible to combine
    /// ranges for multiple shader stages if they are the same.
    ///
    /// The default value is empty.
    pub push_constant_ranges: Vec<PushConstantRange>,

    pub _ne: crate::NonExhaustive,
}

impl Default for PipelineLayoutCreateInfo {
    #[inline]
    fn default() -> Self {
        Self {
            flags: PipelineLayoutCreateFlags::empty(),
            set_layouts: Vec::new(),
            push_constant_ranges: Vec::new(),
            _ne: crate::NonExhaustive(()),
        }
    }
}

impl PipelineLayoutCreateInfo {
    pub(crate) fn validate(&self, device: &Device) -> Result<(), ValidationError> {
        let properties = device.physical_device().properties();

        let &Self {
            flags,
            ref set_layouts,
            ref push_constant_ranges,
            _ne: _,
        } = self;

        flags
            .validate_device(device)
            .map_err(|err| ValidationError {
                context: "flags".into(),
                vuids: &["VUID-VkPipelineLayoutCreateInfo-flags-parameter"],
                ..ValidationError::from_requirement(err)
            })?;

        if set_layouts.len() > properties.max_bound_descriptor_sets as usize {
            return Err(ValidationError {
                context: "set_layouts".into(),
                problem: "the length exceeds the max_bound_descriptor_sets limit".into(),
                vuids: &["VUID-VkPipelineLayoutCreateInfo-setLayoutCount-00286"],
                ..Default::default()
            });
        }

        struct DescriptorLimit {
            descriptor_types: &'static [DescriptorType],
            get_limit: fn(&Properties) -> u32,
            limit_name: &'static str,
            vuids: &'static [&'static str],
        }

        const PER_STAGE_DESCRIPTOR_LIMITS: [DescriptorLimit; 7] = [
            DescriptorLimit {
                descriptor_types: &[
                    DescriptorType::Sampler,
                    DescriptorType::CombinedImageSampler,
                ],
                get_limit: |p| p.max_per_stage_descriptor_samplers,
                limit_name: "max_per_stage_descriptor_samplers",
                vuids: &["VUID-VkPipelineLayoutCreateInfo-descriptorType-03016"],
            },
            DescriptorLimit {
                descriptor_types: &[
                    DescriptorType::UniformBuffer,
                    DescriptorType::UniformBufferDynamic,
                ],
                get_limit: |p| p.max_per_stage_descriptor_uniform_buffers,
                limit_name: "max_per_stage_descriptor_uniform_buffers",
                vuids: &["VUID-VkPipelineLayoutCreateInfo-descriptorType-03017"],
            },
            DescriptorLimit {
                descriptor_types: &[
                    DescriptorType::StorageBuffer,
                    DescriptorType::StorageBufferDynamic,
                ],
                get_limit: |p| p.max_per_stage_descriptor_storage_buffers,
                limit_name: "max_per_stage_descriptor_storage_buffers",
                vuids: &["VUID-VkPipelineLayoutCreateInfo-descriptorType-03018"],
            },
            DescriptorLimit {
                descriptor_types: &[
                    DescriptorType::CombinedImageSampler,
                    DescriptorType::SampledImage,
                    DescriptorType::UniformTexelBuffer,
                ],
                get_limit: |p| p.max_per_stage_descriptor_sampled_images,
                limit_name: "max_per_stage_descriptor_sampled_images",
                vuids: &["VUID-VkPipelineLayoutCreateInfo-descriptorType-06939"],
            },
            DescriptorLimit {
                descriptor_types: &[
                    DescriptorType::StorageImage,
                    DescriptorType::StorageTexelBuffer,
                ],
                get_limit: |p| p.max_per_stage_descriptor_storage_images,
                limit_name: "max_per_stage_descriptor_storage_images",
                vuids: &["VUID-VkPipelineLayoutCreateInfo-descriptorType-03020"],
            },
            DescriptorLimit {
                descriptor_types: &[DescriptorType::InputAttachment],
                get_limit: |p| p.max_per_stage_descriptor_input_attachments,
                limit_name: "max_per_stage_descriptor_input_attachments",
                vuids: &["VUID-VkPipelineLayoutCreateInfo-descriptorType-03021"],
            },
            DescriptorLimit {
                descriptor_types: &[DescriptorType::AccelerationStructure],
                get_limit: |p| {
                    p.max_per_stage_descriptor_acceleration_structures
                        .unwrap_or(0)
                },
                limit_name: "max_per_stage_descriptor_acceleration_structures",
                vuids: &["VUID-VkPipelineLayoutCreateInfo-descriptorType-03571"],
            },
        ];

        const TOTAL_DESCRIPTOR_LIMITS: [DescriptorLimit; 9] = [
            DescriptorLimit {
                descriptor_types: &[
                    DescriptorType::Sampler,
                    DescriptorType::CombinedImageSampler,
                ],
                get_limit: |p| p.max_descriptor_set_samplers,
                limit_name: "max_descriptor_set_samplers",
                vuids: &["VUID-VkPipelineLayoutCreateInfo-descriptorType-03028"],
            },
            DescriptorLimit {
                descriptor_types: &[DescriptorType::UniformBuffer],
                get_limit: |p| p.max_descriptor_set_uniform_buffers,
                limit_name: "max_descriptor_set_uniform_buffers",
                vuids: &["VUID-VkPipelineLayoutCreateInfo-descriptorType-03029"],
            },
            DescriptorLimit {
                descriptor_types: &[DescriptorType::UniformBufferDynamic],
                get_limit: |p| p.max_descriptor_set_uniform_buffers_dynamic,
                limit_name: "max_descriptor_set_uniform_buffers_dynamic",
                vuids: &["VUID-VkPipelineLayoutCreateInfo-descriptorType-03030"],
            },
            DescriptorLimit {
                descriptor_types: &[DescriptorType::StorageBuffer],
                get_limit: |p| p.max_descriptor_set_storage_buffers,
                limit_name: "max_descriptor_set_storage_buffers",
                vuids: &["VUID-VkPipelineLayoutCreateInfo-descriptorType-03031"],
            },
            DescriptorLimit {
                descriptor_types: &[DescriptorType::StorageBufferDynamic],
                get_limit: |p| p.max_descriptor_set_storage_buffers_dynamic,
                limit_name: "max_descriptor_set_storage_buffers_dynamic",
                vuids: &["VUID-VkPipelineLayoutCreateInfo-descriptorType-03032"],
            },
            DescriptorLimit {
                descriptor_types: &[
                    DescriptorType::CombinedImageSampler,
                    DescriptorType::SampledImage,
                    DescriptorType::UniformTexelBuffer,
                ],
                get_limit: |p| p.max_descriptor_set_sampled_images,
                limit_name: "max_descriptor_set_sampled_images",
                vuids: &["VUID-VkPipelineLayoutCreateInfo-descriptorType-03033"],
            },
            DescriptorLimit {
                descriptor_types: &[
                    DescriptorType::StorageImage,
                    DescriptorType::StorageTexelBuffer,
                ],
                get_limit: |p| p.max_descriptor_set_storage_images,
                limit_name: "max_descriptor_set_storage_images",
                vuids: &["VUID-VkPipelineLayoutCreateInfo-descriptorType-03034"],
            },
            DescriptorLimit {
                descriptor_types: &[DescriptorType::InputAttachment],
                get_limit: |p| p.max_descriptor_set_input_attachments,
                limit_name: "max_descriptor_set_input_attachments",
                vuids: &["VUID-VkPipelineLayoutCreateInfo-descriptorType-03035"],
            },
            DescriptorLimit {
                descriptor_types: &[DescriptorType::AccelerationStructure],
                get_limit: |p| p.max_descriptor_set_acceleration_structures.unwrap_or(0),
                limit_name: "max_descriptor_set_acceleration_structures",
                vuids: &["VUID-VkPipelineLayoutCreateInfo-descriptorType-03573"],
            },
        ];

        let mut per_stage_descriptors: [HashMap<ShaderStage, u32>;
            PER_STAGE_DESCRIPTOR_LIMITS.len()] = array::from_fn(|_| HashMap::default());
        let mut total_descriptors = [0; TOTAL_DESCRIPTOR_LIMITS.len()];
        let mut has_push_descriptor_set = false;

        for (_set_num, set_layout) in set_layouts.iter().enumerate() {
            assert_eq!(device, set_layout.device().as_ref());

            if set_layout
                .flags()
                .intersects(DescriptorSetLayoutCreateFlags::PUSH_DESCRIPTOR)
            {
                if has_push_descriptor_set {
                    return Err(ValidationError {
                        context: "set_layouts".into(),
                        problem: "contains more than one descriptor set layout whose flags \
                                DescriptorSetLayoutCreateFlags::PUSH_DESCRIPTOR"
                            .into(),
                        vuids: &["VUID-VkPipelineLayoutCreateInfo-pSetLayouts-00293"],
                        ..Default::default()
                    });
                }

                has_push_descriptor_set = true;
            }

            for layout_binding in set_layout.bindings().values() {
                let &DescriptorSetLayoutBinding {
                    binding_flags: _,
                    descriptor_type,
                    descriptor_count,
                    stages,
                    immutable_samplers: _,
                    _ne: _,
                } = layout_binding;

                for (limit, count) in PER_STAGE_DESCRIPTOR_LIMITS
                    .iter()
                    .zip(&mut per_stage_descriptors)
                {
                    if limit.descriptor_types.contains(&descriptor_type) {
                        for stage in stages {
                            *count.entry(stage).or_default() += descriptor_count;
                        }
                    }
                }

                for (limit, count) in TOTAL_DESCRIPTOR_LIMITS.iter().zip(&mut total_descriptors) {
                    if limit.descriptor_types.contains(&descriptor_type) {
                        *count += descriptor_count;
                    }
                }
            }
        }

        for (limit, count) in PER_STAGE_DESCRIPTOR_LIMITS
            .iter()
            .zip(per_stage_descriptors)
        {
            if let Some((max_stage, max_count)) = count.into_iter().max_by_key(|(_, c)| *c) {
                if max_count > (limit.get_limit)(properties) {
                    return Err(ValidationError {
                        context: "set_layouts".into(),
                        problem: format!(
                            "the combined number of {} descriptors accessible to the \
                            ShaderStage::{:?} stage exceeds the {} limit",
                            limit.descriptor_types[1..].iter().fold(
                                format!("DescriptorType::{:?}", limit.descriptor_types[0]),
                                |mut s, dt| {
                                    write!(s, " + DescriptorType::{:?}", dt).unwrap();
                                    s
                                }
                            ),
                            max_stage,
                            limit.limit_name,
                        )
                        .into(),
                        vuids: limit.vuids,
                        ..Default::default()
                    });
                }
            }
        }

        for (limit, count) in TOTAL_DESCRIPTOR_LIMITS.iter().zip(total_descriptors) {
            if count > (limit.get_limit)(properties) {
                return Err(ValidationError {
                    context: "set_layouts".into(),
                    problem: format!(
                        "the combined number of {} descriptors accessible across all \
                        shader stages exceeds the {} limit",
                        limit.descriptor_types[1..].iter().fold(
                            format!("DescriptorType::{:?}", limit.descriptor_types[0]),
                            |mut s, dt| {
                                write!(s, " + DescriptorType::{:?}", dt).unwrap();
                                s
                            }
                        ),
                        limit.limit_name,
                    )
                    .into(),
                    vuids: limit.vuids,
                    ..Default::default()
                });
            }
        }

        let mut seen_stages = ShaderStages::empty();

        for (range_index, range) in push_constant_ranges.iter().enumerate() {
            range
                .validate(device)
                .map_err(|err| err.add_context(format!("push_constant_ranges[{}]", range_index)))?;

            let &PushConstantRange {
                stages,
                offset: _,
                size: _,
            } = range;

            if seen_stages.intersects(stages) {
                return Err(ValidationError {
                    context: "push_constant_ranges".into(),
                    problem: "contains more than one range with the same stage".into(),
                    vuids: &["VUID-VkPipelineLayoutCreateInfo-pPushConstantRanges-00292"],
                    ..Default::default()
                });
            }

            seen_stages |= stages;
        }

        Ok(())
    }
}

vulkan_bitflags! {
    #[non_exhaustive]

    /// Flags that control how a pipeline layout is created.
    PipelineLayoutCreateFlags = PipelineLayoutCreateFlags(u32);

    /* TODO: enable
    // TODO: document
    INDEPENDENT_SETS = INDEPENDENT_SETS_EXT {
        device_extensions: [ext_graphics_pipeline_library],
    }, */
}

/// Description of a range of the push constants of a pipeline layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PushConstantRange {
    /// The stages which can access this range. A stage can access at most one push constant range.
    ///
    /// The default value is [`ShaderStages::empty()`], which must be overridden.
    pub stages: ShaderStages,

    /// Offset in bytes from the start of the push constants to this range.
    ///
    /// The value must be a multiple of 4.
    ///
    /// The default value is `0`.
    pub offset: u32,

    /// Size in bytes of the range.
    ///
    /// The value must be a multiple of 4, and not 0.
    ///
    /// The default value is `0`, which must be overridden.
    pub size: u32,
}

impl Default for PushConstantRange {
    #[inline]
    fn default() -> Self {
        Self {
            stages: ShaderStages::empty(),
            offset: 0,
            size: 0,
        }
    }
}

impl PushConstantRange {
    pub(crate) fn validate(&self, device: &Device) -> Result<(), ValidationError> {
        let &Self {
            stages,
            offset,
            size,
        } = self;

        stages
            .validate_device(device)
            .map_err(|err| ValidationError {
                context: "stages".into(),
                vuids: &["VUID-VkPushConstantRange-stageFlags-parameter"],
                ..ValidationError::from_requirement(err)
            })?;

        if stages.is_empty() {
            return Err(ValidationError {
                context: "stages".into(),
                problem: "is empty".into(),
                vuids: &["VUID-VkPushConstantRange-stageFlags-requiredbitmask"],
                ..Default::default()
            });
        }

        let max_push_constants_size = device
            .physical_device()
            .properties()
            .max_push_constants_size;

        if offset >= max_push_constants_size {
            return Err(ValidationError {
                context: "offset".into(),
                problem: "is not less than the max_push_constants_size limit".into(),
                vuids: &["VUID-VkPushConstantRange-offset-00294"],
                ..Default::default()
            });
        }

        if offset % 4 != 0 {
            return Err(ValidationError {
                context: "offset".into(),
                problem: "is not a multiple of 4".into(),
                vuids: &["VUID-VkPushConstantRange-offset-00295"],
                ..Default::default()
            });
        }

        if size == 0 {
            return Err(ValidationError {
                context: "size".into(),
                problem: "is zero".into(),
                vuids: &["VUID-VkPushConstantRange-size-00296"],
                ..Default::default()
            });
        }

        if size % 4 != 0 {
            return Err(ValidationError {
                context: "size".into(),
                problem: "is not a multiple of 4".into(),
                vuids: &["VUID-VkPushConstantRange-size-00297"],
                ..Default::default()
            });
        }

        if size > max_push_constants_size - offset {
            return Err(ValidationError {
                problem: "size is greater than max_push_constants_size limit minus offset".into(),
                vuids: &["VUID-VkPushConstantRange-size-00298"],
                ..Default::default()
            });
        }

        Ok(())
    }
}

/// Error when checking whether a pipeline layout is a superset of another one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PipelineLayoutSupersetError {
    DescriptorMissing {
        set_num: u32,
        binding_num: u32,
    },
    DescriptorRequirementsNotMet {
        set_num: u32,
        binding_num: u32,
        error: DescriptorRequirementsNotMet,
    },
    PushConstantRange {
        first_range: PushConstantRange,
        second_range: PushConstantRange,
    },
}

impl Error for PipelineLayoutSupersetError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            PipelineLayoutSupersetError::DescriptorRequirementsNotMet { error, .. } => Some(error),
            _ => None,
        }
    }
}

impl Display for PipelineLayoutSupersetError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FmtError> {
        match self {
            PipelineLayoutSupersetError::DescriptorRequirementsNotMet {
                set_num,
                binding_num,
                ..
            } => write!(
                f,
                "the descriptor at set {} binding {} does not meet the requirements",
                set_num, binding_num,
            ),
            PipelineLayoutSupersetError::DescriptorMissing {
                set_num,
                binding_num,
            } => write!(
                f,
                "a descriptor at set {} binding {} is required by the shaders, but is missing from \
                the pipeline layout",
                set_num, binding_num,
            ),
            PipelineLayoutSupersetError::PushConstantRange {
                first_range,
                second_range,
            } => {
                writeln!(f, "our range did not completely encompass the other range")?;
                writeln!(f, "    our stages: {:?}", first_range.stages)?;
                writeln!(
                    f,
                    "    our range: {} - {}",
                    first_range.offset,
                    first_range.offset + first_range.size,
                )?;
                writeln!(f, "    other stages: {:?}", second_range.stages)?;
                write!(
                    f,
                    "    other range: {} - {}",
                    second_range.offset,
                    second_range.offset + second_range.size,
                )
            }
        }
    }
}

/// Parameters to create a new `PipelineLayout` as well as its accompanying `DescriptorSetLayout`
/// objects.
#[derive(Clone, Debug)]
pub struct PipelineDescriptorSetLayoutCreateInfo {
    pub flags: PipelineLayoutCreateFlags,
    pub set_layouts: Vec<DescriptorSetLayoutCreateInfo>,
    pub push_constant_ranges: Vec<PushConstantRange>,
}

impl PipelineDescriptorSetLayoutCreateInfo {
    /// Creates a new `PipelineDescriptorSetLayoutCreateInfo` from the union of the requirements of
    /// each shader stage in `stages`.
    pub fn from_stages<'a>(
        stages: impl IntoIterator<Item = &'a PipelineShaderStageCreateInfo>,
    ) -> Self {
        // Produce `DescriptorBindingRequirements` for each binding, by iterating over all
        // shaders and adding the requirements of each.
        let mut descriptor_binding_requirements: HashMap<
            (u32, u32),
            DescriptorBindingRequirements,
        > = HashMap::default();
        let mut max_set_num = 0;
        let mut push_constant_ranges: Vec<PushConstantRange> = Vec::new();

        for stage in stages {
            let entry_point_info = stage.entry_point.info();

            for (&(set_num, binding_num), reqs) in &entry_point_info.descriptor_binding_requirements
            {
                max_set_num = max(max_set_num, set_num);

                match descriptor_binding_requirements.entry((set_num, binding_num)) {
                    Entry::Occupied(entry) => {
                        // Previous shaders already added requirements, so we merge
                        // requirements of the current shader into the requirements of the
                        // previous ones.
                        // TODO: return an error here instead of panicking?
                        entry.into_mut().merge(reqs).expect("Could not produce an intersection of the shader descriptor requirements");
                    }
                    Entry::Vacant(entry) => {
                        // No previous shader had this descriptor yet, so we just insert the
                        // requirements.
                        entry.insert(reqs.clone());
                    }
                }
            }

            if let Some(range) = &entry_point_info.push_constant_requirements {
                if let Some(existing_range) =
                    push_constant_ranges.iter_mut().find(|existing_range| {
                        existing_range.offset == range.offset && existing_range.size == range.size
                    })
                {
                    // If this range was already used before, add our stage to it.
                    existing_range.stages |= range.stages;
                } else {
                    // If this range is new, insert it.
                    push_constant_ranges.push(*range)
                }
            }
        }

        // Convert the descriptor binding requirements.
        let mut set_layouts =
            vec![DescriptorSetLayoutCreateInfo::default(); max_set_num as usize + 1];

        for ((set_num, binding_num), reqs) in descriptor_binding_requirements {
            set_layouts[set_num as usize]
                .bindings
                .insert(binding_num, DescriptorSetLayoutBinding::from(&reqs));
        }

        Self {
            flags: PipelineLayoutCreateFlags::empty(),
            set_layouts,
            push_constant_ranges,
        }
    }

    /// Converts the `PipelineDescriptorSetLayoutCreateInfo` into a `PipelineLayoutCreateInfo` by
    /// creating the descriptor set layout objects.
    pub fn into_pipeline_layout_create_info(
        self,
        device: Arc<Device>,
    ) -> Result<PipelineLayoutCreateInfo, IntoPipelineLayoutCreateInfoError> {
        let PipelineDescriptorSetLayoutCreateInfo {
            flags,
            set_layouts,
            push_constant_ranges,
        } = self;

        let set_layouts = set_layouts
            .into_iter()
            .enumerate()
            .map(|(set_num, create_info)| {
                DescriptorSetLayout::new(device.clone(), create_info).map_err(|error| {
                    IntoPipelineLayoutCreateInfoError {
                        set_num: set_num as u32,
                        error,
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(PipelineLayoutCreateInfo {
            flags,
            set_layouts,
            push_constant_ranges,
            _ne: crate::NonExhaustive(()),
        })
    }
}

#[derive(Clone, Debug)]
pub struct IntoPipelineLayoutCreateInfoError {
    pub set_num: u32,
    pub error: VulkanError,
}

impl Display for IntoPipelineLayoutCreateInfoError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "an error occurred while creating a descriptor set layout for set number {}",
            self.set_num
        )
    }
}

impl Error for IntoPipelineLayoutCreateInfoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.error)
    }
}

#[cfg(test)]
mod tests {

    use super::PipelineLayout;
    use crate::{
        pipeline::layout::{PipelineLayoutCreateInfo, PushConstantRange},
        shader::ShaderStages,
    };

    #[test]
    fn push_constant_ranges_disjoint() {
        let test_cases = [
            // input:
            // - `0..12`, stage=fragment
            // - `0..40`, stage=vertex
            //
            // output:
            // - `0..12`, stage=fragment|vertex
            // - `12..40`, stage=vertex
            (
                &[
                    PushConstantRange {
                        stages: ShaderStages::FRAGMENT,
                        offset: 0,
                        size: 12,
                    },
                    PushConstantRange {
                        stages: ShaderStages::VERTEX,
                        offset: 0,
                        size: 40,
                    },
                ][..],
                &[
                    PushConstantRange {
                        stages: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                        offset: 0,
                        size: 12,
                    },
                    PushConstantRange {
                        stages: ShaderStages::VERTEX,
                        offset: 12,
                        size: 28,
                    },
                ][..],
            ),
            // input:
            // - `0..12`, stage=fragment
            // - `4..40`, stage=vertex
            //
            // output:
            // - `0..4`, stage=fragment
            // - `4..12`, stage=fragment|vertex
            // - `12..40`, stage=vertex
            (
                &[
                    PushConstantRange {
                        stages: ShaderStages::FRAGMENT,
                        offset: 0,
                        size: 12,
                    },
                    PushConstantRange {
                        stages: ShaderStages::VERTEX,
                        offset: 4,
                        size: 36,
                    },
                ][..],
                &[
                    PushConstantRange {
                        stages: ShaderStages::FRAGMENT,
                        offset: 0,
                        size: 4,
                    },
                    PushConstantRange {
                        stages: ShaderStages::FRAGMENT | ShaderStages::VERTEX,
                        offset: 4,
                        size: 8,
                    },
                    PushConstantRange {
                        stages: ShaderStages::VERTEX,
                        offset: 12,
                        size: 28,
                    },
                ][..],
            ),
            // input:
            // - `0..12`, stage=fragment
            // - `8..20`, stage=compute
            // - `4..16`, stage=vertex
            // - `8..32`, stage=tess_ctl
            //
            // output:
            // - `0..4`, stage=fragment
            // - `4..8`, stage=fragment|vertex
            // - `8..16`, stage=fragment|vertex|compute|tess_ctl
            // - `16..20`, stage=compute|tess_ctl
            // - `20..32` stage=tess_ctl
            (
                &[
                    PushConstantRange {
                        stages: ShaderStages::FRAGMENT,
                        offset: 0,
                        size: 12,
                    },
                    PushConstantRange {
                        stages: ShaderStages::COMPUTE,
                        offset: 8,
                        size: 12,
                    },
                    PushConstantRange {
                        stages: ShaderStages::VERTEX,
                        offset: 4,
                        size: 12,
                    },
                    PushConstantRange {
                        stages: ShaderStages::TESSELLATION_CONTROL,
                        offset: 8,
                        size: 24,
                    },
                ][..],
                &[
                    PushConstantRange {
                        stages: ShaderStages::FRAGMENT,
                        offset: 0,
                        size: 4,
                    },
                    PushConstantRange {
                        stages: ShaderStages::FRAGMENT | ShaderStages::VERTEX,
                        offset: 4,
                        size: 4,
                    },
                    PushConstantRange {
                        stages: ShaderStages::VERTEX
                            | ShaderStages::FRAGMENT
                            | ShaderStages::COMPUTE
                            | ShaderStages::TESSELLATION_CONTROL,
                        offset: 8,
                        size: 4,
                    },
                    PushConstantRange {
                        stages: ShaderStages::VERTEX
                            | ShaderStages::COMPUTE
                            | ShaderStages::TESSELLATION_CONTROL,
                        offset: 12,
                        size: 4,
                    },
                    PushConstantRange {
                        stages: ShaderStages::COMPUTE | ShaderStages::TESSELLATION_CONTROL,
                        offset: 16,
                        size: 4,
                    },
                    PushConstantRange {
                        stages: ShaderStages::TESSELLATION_CONTROL,
                        offset: 20,
                        size: 12,
                    },
                ][..],
            ),
        ];

        let (device, _) = gfx_dev_and_queue!();

        for (input, expected) in test_cases {
            let layout = PipelineLayout::new(
                device.clone(),
                PipelineLayoutCreateInfo {
                    push_constant_ranges: input.into(),
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(layout.push_constant_ranges_disjoint.as_slice(), expected);
        }
    }
}

/* TODO: restore
#[cfg(test)]
mod tests {
    use std::iter;
    use std::sync::Arc;
    use descriptor::descriptor::ShaderStages;
    use descriptor::descriptor_set::DescriptorSetLayout;
    use descriptor::pipeline_layout::sys::PipelineLayout;
    use descriptor::pipeline_layout::sys::PipelineLayoutCreationError;

    #[test]
    fn empty() {
        let (device, _) = gfx_dev_and_queue!();
        let _layout = PipelineLayout::new(&device, iter::empty(), iter::empty()).unwrap();
    }

    #[test]
    fn wrong_device_panic() {
        let (device1, _) = gfx_dev_and_queue!();
        let (device2, _) = gfx_dev_and_queue!();

        let set = match DescriptorSetLayout::raw(device1, iter::empty()) {
            Ok(s) => Arc::new(s),
            Err(_) => return
        };

        assert_should_panic!({
            let _ = PipelineLayout::new(&device2, Some(&set), iter::empty());
        });
    }

    #[test]
    fn invalid_push_constant_stages() {
        let (device, _) = gfx_dev_and_queue!();

        let push_constant = (0, 8, ShaderStages::empty());

        match PipelineLayout::new(&device, iter::empty(), Some(push_constant)) {
            Err(PipelineLayoutCreationError::InvalidPushConstant) => (),
            _ => panic!()
        }
    }

    #[test]
    fn invalid_push_constant_size1() {
        let (device, _) = gfx_dev_and_queue!();

        let push_constant = (0, 0, ShaderStages::all_graphics());

        match PipelineLayout::new(&device, iter::empty(), Some(push_constant)) {
            Err(PipelineLayoutCreationError::InvalidPushConstant) => (),
            _ => panic!()
        }
    }

    #[test]
    fn invalid_push_constant_size2() {
        let (device, _) = gfx_dev_and_queue!();

        let push_constant = (0, 11, ShaderStages::all_graphics());

        match PipelineLayout::new(&device, iter::empty(), Some(push_constant)) {
            Err(PipelineLayoutCreationError::InvalidPushConstant) => (),
            _ => panic!()
        }
    }
}
*/
