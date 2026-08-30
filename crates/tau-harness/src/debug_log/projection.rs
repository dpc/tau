//! Borrowed serialization that bounds Provider debug string values and images.
//!
//! The established projection leaves JSON object keys and generic CBOR byte
//! arrays unchanged. They still enter the final diagnostic
//! [`serde_json::Value`] because changing them would change the existing debug
//! bytes. The adapter prevents ordinary content-sized string values and typed
//! provider-image bytes from entering that tree.

use serde::ser::{
    SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple,
    SerializeTupleStruct, SerializeTupleVariant,
};
use serde::{Serialize, Serializer};
use tau_proto::{ContextItem, ProviderResponseFinished, ToolResultContentPart, ToolResultItem};

use super::{DEBUG_STRING_COMPACT_THRESHOLD, compact_debug_string};

/// Serializes a value while bounding every string value before the destination
/// owns it.
pub(super) struct CompactStrings<T>(pub(super) T);

impl<T: Serialize> Serialize for CompactStrings<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(CompactStringsSerializer(serializer))
    }
}

/// A serde adapter that bounds string values and otherwise preserves the source
/// shape.
///
/// Map keys deliberately bypass this adapter to match the previous recursive
/// [`serde_json::Value`] projection, which compacted values but not keys.
struct CompactStringsSerializer<S>(S);

macro_rules! forward_scalar {
    ($name:ident($ty:ty)) => {
        fn $name(self, value: $ty) -> Result<Self::Ok, Self::Error> {
            self.0.$name(value)
        }
    };
}

impl<S: Serializer> Serializer for CompactStringsSerializer<S> {
    type Ok = S::Ok;
    type Error = S::Error;
    type SerializeSeq = CompactCompound<S::SerializeSeq>;
    type SerializeTuple = CompactCompound<S::SerializeTuple>;
    type SerializeTupleStruct = CompactCompound<S::SerializeTupleStruct>;
    type SerializeTupleVariant = CompactCompound<S::SerializeTupleVariant>;
    type SerializeMap = CompactCompound<S::SerializeMap>;
    type SerializeStruct = CompactCompound<S::SerializeStruct>;
    type SerializeStructVariant = CompactCompound<S::SerializeStructVariant>;

    forward_scalar!(serialize_bool(bool));
    forward_scalar!(serialize_i8(i8));
    forward_scalar!(serialize_i16(i16));
    forward_scalar!(serialize_i32(i32));
    forward_scalar!(serialize_i64(i64));
    forward_scalar!(serialize_i128(i128));
    forward_scalar!(serialize_u8(u8));
    forward_scalar!(serialize_u16(u16));
    forward_scalar!(serialize_u32(u32));
    forward_scalar!(serialize_u64(u64));
    forward_scalar!(serialize_u128(u128));
    forward_scalar!(serialize_f32(f32));
    forward_scalar!(serialize_f64(f64));
    forward_scalar!(serialize_char(char));

    fn serialize_str(self, value: &str) -> Result<Self::Ok, Self::Error> {
        if value.len() <= DEBUG_STRING_COMPACT_THRESHOLD {
            self.0.serialize_str(value)
        } else {
            self.0.serialize_str(&compact_debug_string(value))
        }
    }

    fn serialize_bytes(self, value: &[u8]) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_bytes(value)
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_none()
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_some(&CompactStrings(value))
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_unit()
    }

    fn serialize_unit_struct(self, name: &'static str) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_unit_struct(name)
    }

    fn serialize_unit_variant(
        self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_unit_variant(name, variant_index, variant)
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        self.0
            .serialize_newtype_struct(name, &CompactStrings(value))
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        self.0
            .serialize_newtype_variant(name, variant_index, variant, &CompactStrings(value))
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        self.0.serialize_seq(len).map(CompactCompound)
    }

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        self.0.serialize_tuple(len).map(CompactCompound)
    }

    fn serialize_tuple_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        self.0
            .serialize_tuple_struct(name, len)
            .map(CompactCompound)
    }

    fn serialize_tuple_variant(
        self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        self.0
            .serialize_tuple_variant(name, variant_index, variant, len)
            .map(CompactCompound)
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        self.0.serialize_map(len).map(CompactCompound)
    }

    fn serialize_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        self.0.serialize_struct(name, len).map(CompactCompound)
    }

    fn serialize_struct_variant(
        self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        self.0
            .serialize_struct_variant(name, variant_index, variant, len)
            .map(CompactCompound)
    }

    fn collect_str<T: ?Sized + std::fmt::Display>(
        self,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        self.serialize_str(&value.to_string())
    }
}

/// Wraps compound serializer children with the same string-bounding adapter.
struct CompactCompound<C>(C);

impl<C: SerializeSeq> SerializeSeq for CompactCompound<C> {
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.0.serialize_element(&CompactStrings(value))
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<C: SerializeTuple> SerializeTuple for CompactCompound<C> {
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.0.serialize_element(&CompactStrings(value))
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<C: SerializeTupleStruct> SerializeTupleStruct for CompactCompound<C> {
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.0.serialize_field(&CompactStrings(value))
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<C: SerializeTupleVariant> SerializeTupleVariant for CompactCompound<C> {
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.0.serialize_field(&CompactStrings(value))
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<C: SerializeMap> SerializeMap for CompactCompound<C> {
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), Self::Error> {
        self.0.serialize_key(key)
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.0.serialize_value(&CompactStrings(value))
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

/// Builds the bounded Provider event object from borrowed protocol fields.
pub(super) fn provider_event_value(
    event_name: tau_proto::EventName,
    event: ProviderEvent<'_>,
) -> serde_json::Value {
    serde_json::to_value(CompactStrings(ProviderEventEnvelope {
        event: event_name,
        payload: event,
    }))
    .unwrap_or_default()
}

/// A Provider debug payload selected without cloning its protocol event.
pub(super) enum ProviderEvent<'a> {
    /// A raw or canonical streaming update.
    Updated(&'a tau_proto::ProviderResponseUpdated),
    /// A raw or canonical terminal response.
    Finished(&'a ProviderResponseFinished),
}

/// The established tagged Provider event shape.
#[derive(Serialize)]
struct ProviderEventEnvelope<'a> {
    /// Raw or canonical event name.
    event: tau_proto::EventName,
    /// Borrowed Provider payload.
    payload: ProviderEvent<'a>,
}

impl Serialize for ProviderEvent<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Updated(updated) => updated.serialize(serializer),
            Self::Finished(finished) => {
                ProviderResponseFinishedProjection(finished).serialize(serializer)
            }
        }
    }
}

/// A borrowed terminal response that substitutes redacted context items.
struct ProviderResponseFinishedProjection<'a>(&'a ProviderResponseFinished);

impl Serialize for ProviderResponseFinishedProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let ProviderResponseFinished {
            agent_prompt_id,
            agent_id,
            output_items,
            stop_reason,
            error,
            failure_kind,
            context_limit_telemetry,
            recovery_disposition,
            output_length_disposition,
            provider_attempt,
            automatic_compaction_decision,
            originator,
            usage,
            estimated_api_cost_rates,
            estimated_api_cost_increment,
            compaction_original_input_tokens,
            compaction_output_tokens,
            backend,
            provider_response_id,
            ws_pool_delta,
        } = self.0;
        let mut state = serializer.serialize_struct("ProviderResponseFinished", 20)?;
        state.serialize_field("agent_prompt_id", agent_prompt_id)?;
        state.serialize_field("agent_id", agent_id)?;
        if !output_items.is_empty() {
            state.serialize_field("output_items", &ContextItemsProjection(output_items))?;
        }
        state.serialize_field("stop_reason", stop_reason)?;
        if let Some(error) = error {
            state.serialize_field("error", error)?;
        }
        if let Some(failure_kind) = failure_kind {
            state.serialize_field("failure_kind", failure_kind)?;
        }
        if let Some(telemetry) = context_limit_telemetry {
            state.serialize_field("context_limit_telemetry", telemetry)?;
        }
        if recovery_disposition != &tau_proto::ContextRecoveryDisposition::default() {
            state.serialize_field("recovery_disposition", recovery_disposition)?;
        }
        if output_length_disposition != &tau_proto::OutputLengthDisposition::default() {
            state.serialize_field("output_length_disposition", output_length_disposition)?;
        }
        if provider_attempt != &tau_proto::ProviderAttempt::default() {
            state.serialize_field("provider_attempt", provider_attempt)?;
        }
        if let Some(decision) = automatic_compaction_decision {
            state.serialize_field("automatic_compaction_decision", decision)?;
        }
        state.serialize_field("originator", originator)?;
        if let Some(usage) = usage {
            state.serialize_field("usage", usage)?;
        }
        if let Some(rates) = estimated_api_cost_rates {
            state.serialize_field("estimated_api_cost_rates", rates)?;
        }
        if let Some(increment) = estimated_api_cost_increment {
            state.serialize_field("estimated_api_cost_increment", increment)?;
        }
        if let Some(tokens) = compaction_original_input_tokens {
            state.serialize_field("compaction_original_input_tokens", tokens)?;
        }
        if let Some(tokens) = compaction_output_tokens {
            state.serialize_field("compaction_output_tokens", tokens)?;
        }
        if let Some(backend) = backend {
            state.serialize_field("backend", backend)?;
        }
        if let Some(response_id) = provider_response_id {
            state.serialize_field("provider_response_id", response_id)?;
        }
        if let Some(delta) = ws_pool_delta {
            state.serialize_field("ws_pool_delta", delta)?;
        }
        state.end()
    }
}

/// Borrowed terminal output items with narrow image-byte redaction.
struct ContextItemsProjection<'a>(&'a [ContextItem]);

impl Serialize for ContextItemsProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for item in self.0 {
            sequence.serialize_element(&ContextItemProjection(item))?;
        }
        sequence.end()
    }
}

/// One borrowed context item, rewrapping only tool results that can hold
/// images.
struct ContextItemProjection<'a>(&'a ContextItem);

impl Serialize for ContextItemProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let ContextItem::ToolResult(result) = self.0 else {
            return self.0.serialize(serializer);
        };
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("type", "tool_result")?;
        map.serialize_entry("payload", &ToolResultProjection(result))?;
        map.end()
    }
}

/// A borrowed tool result whose provider images serialize with empty data.
struct ToolResultProjection<'a>(&'a ToolResultItem);

impl Serialize for ToolResultProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let ToolResultItem {
            call_id,
            tool_type,
            status,
            output,
            presentation,
            provider_content,
        } = self.0;
        let mut state = serializer.serialize_struct("ToolResultItem", 6)?;
        state.serialize_field("call_id", call_id)?;
        state.serialize_field("tool_type", tool_type)?;
        state.serialize_field("status", status)?;
        state.serialize_field("output", output)?;
        state.serialize_field("presentation", presentation)?;
        if !provider_content.is_empty() {
            state.serialize_field(
                "provider_content",
                &ProviderContentProjection(provider_content),
            )?;
        }
        state.end()
    }
}

/// Borrowed provider content with all image payload bytes cleared.
struct ProviderContentProjection<'a>(&'a [ToolResultContentPart]);

impl Serialize for ProviderContentProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for part in self.0 {
            let ToolResultContentPart::Image(image) = part;
            sequence.serialize_element(&ImageProjection(image))?;
        }
        sequence.end()
    }
}

/// The established tagged image shape with an empty byte array.
struct ImageProjection<'a>(&'a tau_proto::ImageContent);

impl Serialize for ImageProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct Content<'a> {
            media_type: tau_proto::ImageMediaType,
            data: &'a [u8],
            width: u32,
            height: u32,
            detail: tau_proto::ImageDetail,
        }
        #[derive(Serialize)]
        struct Tagged<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            content: Content<'a>,
        }
        Tagged {
            kind: "image",
            content: Content {
                media_type: self.0.media_type,
                data: &[],
                width: self.0.width,
                height: self.0.height,
                detail: self.0.detail,
            },
        }
        .serialize(serializer)
    }
}

impl<C: SerializeStruct> SerializeStruct for CompactCompound<C> {
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        self.0.serialize_field(key, &CompactStrings(value))
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<C: SerializeStructVariant> SerializeStructVariant for CompactCompound<C> {
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        self.0.serialize_field(key, &CompactStrings(value))
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}
