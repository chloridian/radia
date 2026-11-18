use super::{AllocationStrategy, TypedMessage};
use crate::{
    lludp::{BlockInstance, FieldValue, error::LLUdpError, schema_message::SchemaMessage, types::MessageId},
    messages::{
        ExtraParameter,
        agent::{
            avatar_appearance::{AppearanceData, AppearanceHover, AttachmentBlock, VisualParam},
            logout_reply::InventoryData,
            teleport_failed::AlertInfo,
        },
        circuit::packet_ack::Packets,
        generic_message::Parameter,
        map::map_block_reply::Data,
        object::{
            improved_terse_object_update, kill_object, object_extra_params, object_update, object_update_cached, object_update_compressed,
            request_multiple_objects,
        },
        parcel::parcel_properties::ParcelData,
        region::{
            region_handshake::RegionInfo4,
            region_info::{CombatSettings, RegionInfo3, RegionInfo5},
        },
    },
    schema,
};

const _: () = {
    assert!(size_of::<RegionInfo4>() <= 512);
    assert!(size_of::<Packets>() <= 512);
    assert!(size_of::<InventoryData>() <= 512);
    assert!(size_of::<Data>() <= 512);
    assert!(size_of::<ExtraParameter>() <= 32);
    assert!(size_of::<object_update::ObjectData>() <= 512);
    assert!(size_of::<object_update_compressed::ObjectData>() <= 512);
    assert!(size_of::<improved_terse_object_update::ObjectData>() <= 512);
    assert!(size_of::<improved_terse_object_update::ObjectBlock>() <= 512);
    assert!(size_of::<object_update_cached::CachedObjectReference>() <= 512);
    assert!(size_of::<kill_object::ObjectData>() <= 512);
    assert!(size_of::<request_multiple_objects::ObjectRequest>() <= 512);
    assert!(size_of::<object_extra_params::ExtraParamUpdate>() <= 512);
    assert!(size_of::<Packets>() <= size_of::<BlockInstance>() + size_of::<FieldValue>());
    assert!(size_of::<InventoryData>() <= size_of::<BlockInstance>() + size_of::<FieldValue>());
    assert!(size_of::<Data>() <= size_of::<BlockInstance>() + 9 * size_of::<FieldValue>());
    assert!(size_of::<object_update_cached::CachedObjectReference>() <= size_of::<BlockInstance>() + 3 * size_of::<FieldValue>());
    assert!(size_of::<kill_object::ObjectData>() <= size_of::<BlockInstance>() + size_of::<FieldValue>());
    assert!(size_of::<request_multiple_objects::ObjectRequest>() <= size_of::<BlockInstance>() + 2 * size_of::<FieldValue>());
    assert!(size_of::<VisualParam>() <= size_of::<BlockInstance>() + size_of::<FieldValue>());
    assert!(size_of::<AppearanceData>() <= size_of::<BlockInstance>() + 3 * size_of::<FieldValue>());
    assert!(size_of::<AppearanceHover>() <= size_of::<BlockInstance>() + size_of::<FieldValue>());
    assert!(size_of::<AttachmentBlock>() <= size_of::<BlockInstance>() + 2 * size_of::<FieldValue>());
    assert!(size_of::<Parameter>() <= size_of::<BlockInstance>() + size_of::<FieldValue>());
    assert!(size_of::<ParcelData>() <= 60 * size_of::<FieldValue>());
    assert!(size_of::<AlertInfo>() <= 2 * size_of::<FieldValue>());
    assert!(size_of::<RegionInfo3>() <= size_of::<FieldValue>());
    assert!(size_of::<RegionInfo5>() <= 7 * size_of::<FieldValue>());
    assert!(size_of::<CombatSettings>() <= 6 * size_of::<FieldValue>());
};

pub(crate) fn typed_allocation_bound<T: TypedMessage>(payload: &[u8]) -> Result<(AllocationStrategy, usize), LLUdpError> {
    let bound = match T::ALLOCATION {
        AllocationStrategy::NoAlloc | AllocationStrategy::Opaque => payload.len(),
        AllocationStrategy::SchemaDerived => {
            let metadata = schema::find_by_id(T::ID).ok_or(LLUdpError::InvalidMessageId)?;
            SchemaMessage::allocation_bound(metadata, payload)?
        }
        AllocationStrategy::Specialized => T::preflight_allocation_bound(payload)?,
    };
    Ok((T::ALLOCATION, bound))
}

pub(crate) fn fallback_allocation_bound(id: MessageId, payload: &[u8]) -> Result<(AllocationStrategy, usize), LLUdpError> {
    if let Some(metadata) = schema::find_by_id(id) {
        Ok((AllocationStrategy::SchemaDerived, SchemaMessage::allocation_bound(metadata, payload)?))
    } else {
        Ok((AllocationStrategy::Opaque, payload.len()))
    }
}

pub(crate) fn capacity_bytes(capacity: usize, element_size: usize) -> usize {
    capacity.saturating_mul(element_size)
}

pub(crate) fn with_payload(payload: &[u8], additional: usize) -> Result<usize, LLUdpError> {
    payload.len().checked_add(additional).ok_or(LLUdpError::AllocationBoundOverflow)
}

pub(crate) fn add_capacity(total: &mut usize, capacity: usize, element_size: usize) {
    *total = total.saturating_add(capacity_bytes(capacity, element_size));
}

pub(crate) fn add_string_capacity(total: &mut usize, value: &String) {
    add_capacity(total, value.capacity(), size_of::<u8>());
}

pub(crate) fn add_extra_parameter_capacity(total: &mut usize, parameters: &Vec<ExtraParameter>) {
    add_capacity(total, parameters.capacity(), size_of::<ExtraParameter>());
    for parameter in parameters {
        add_capacity(total, parameter.data.capacity(), size_of::<u8>());
    }
}

pub(crate) fn add_object_update_capacity(total: &mut usize, object: &object_update::ObjectData) {
    add_capacity(total, object.texture_entry.capacity(), size_of::<u8>());
    add_capacity(total, object.texture_anim.capacity(), size_of::<u8>());
    add_capacity(total, object.name_value.capacity(), size_of::<u8>());
    add_capacity(total, object.data.capacity(), size_of::<u8>());
    add_string_capacity(total, &object.text.text);
    add_string_capacity(total, &object.media_url);
    add_capacity(total, object.particle_system.capacity(), size_of::<u8>());
    add_extra_parameter_capacity(total, &object.extra_parameters);
}

pub(crate) fn add_object_update_compressed_capacity(total: &mut usize, object: &object_update_compressed::ObjectData) {
    if let Some(object_update_compressed::ObjectSpecificData::ScratchPad(bytes)) = &object.data {
        add_capacity(total, bytes.capacity(), size_of::<u8>());
    }
    if let Some(text) = &object.text {
        add_string_capacity(total, &text.text);
    }
    if let Some(media_url) = &object.media_url {
        add_string_capacity(total, media_url);
    }
    if let Some(bytes) = &object.legacy_particle_system {
        add_capacity(total, bytes.capacity(), size_of::<u8>());
    }
    add_extra_parameter_capacity(total, &object.extra_parameters);
    if let Some(name_value) = &object.name_value {
        add_string_capacity(total, name_value);
    }
    if let Some(bytes) = &object.texture_entry {
        add_capacity(total, bytes.capacity(), size_of::<u8>());
    }
    if let Some(bytes) = &object.texture_animation {
        add_capacity(total, bytes.capacity(), size_of::<u8>());
    }
    if let Some(bytes) = &object.particle_system {
        add_capacity(total, bytes.capacity(), size_of::<u8>());
    }
}
