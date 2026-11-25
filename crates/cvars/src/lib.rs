use std::{array, borrow::Cow, error::Error, fmt, marker::PhantomData, sync::Arc};

include!(concat!(env!("OUT_DIR"), "/generated_cvars.rs"));

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CvarId(pub(crate) usize);

impl CvarId {
    pub const fn index(self) -> usize {
        self.0
    }

    pub fn descriptor(self) -> &'static CvarDescriptor {
        &DESCRIPTORS[self.0]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CvarKey<T> {
    id: CvarId,
    _marker: PhantomData<fn() -> T>,
}

impl<T> CvarKey<T> {
    pub const fn new(id: CvarId) -> Self {
        Self { id, _marker: PhantomData }
    }

    pub const fn id(self) -> CvarId {
        self.id
    }

    pub fn descriptor(self) -> &'static CvarDescriptor {
        self.id.descriptor()
    }
}

impl<T: CvarTyped> CvarKey<T> {
    pub fn default_value(self) -> T {
        T::from_value(&self.descriptor().default).expect("generated cvar default matches CvarKey type")
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Layer {
    Default,
    Preset,
    Global,
    StartupDeveloper,
    Session,
}

impl Layer {
    const OVERRIDE_LAYERS: [Self; 4] = [Self::Preset, Self::Global, Self::StartupDeveloper, Self::Session];

    const fn slot(self) -> Option<usize> {
        match self {
            Self::Default => None,
            Self::Preset => Some(0),
            Self::Global => Some(1),
            Self::StartupDeveloper => Some(2),
            Self::Session => Some(3),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistenceScope {
    Global,
    SessionOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyScope {
    Live,
    RestartRequired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Visibility {
    Visible,
    Hidden,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueDomain {
    Bool,
    I64 { min: i64, max: i64 },
    F32 { min: f32, max: f32, step: Option<f32> },
    F64 { min: f64, max: f64, step: Option<f64> },
    String { max_len: usize },
    Enum { variants: &'static [&'static str] },
}

impl ValueDomain {
    pub const fn type_name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::I64 { .. } => "i64",
            Self::F32 { .. } => "f32",
            Self::F64 { .. } => "f64",
            Self::String { .. } => "string",
            Self::Enum { .. } => "enum",
        }
    }

    pub fn validate(self, name: &'static str, value: &CvarValue) -> Result<CvarValue, CvarError> {
        match (self, value) {
            (Self::Bool, CvarValue::Bool(flag)) => Ok(CvarValue::Bool(*flag)),
            (Self::I64 { min, max }, CvarValue::I64(number)) => {
                if (*number < min) || (*number > max) {
                    Err(CvarError::OutOfBounds { name })
                } else {
                    Ok(CvarValue::I64(*number))
                }
            }
            (Self::F32 { min, max, .. }, CvarValue::F32(number)) => {
                if !number.is_finite() {
                    Err(CvarError::NonFinite { name })
                } else if (*number < min) || (*number > max) {
                    Err(CvarError::OutOfBounds { name })
                } else {
                    Ok(CvarValue::F32(*number))
                }
            }
            (Self::F64 { min, max, .. }, CvarValue::F64(number)) => {
                if !number.is_finite() {
                    Err(CvarError::NonFinite { name })
                } else if (*number < min) || (*number > max) {
                    Err(CvarError::OutOfBounds { name })
                } else {
                    Ok(CvarValue::F64(*number))
                }
            }
            (Self::String { max_len }, CvarValue::String(text)) => {
                if text.len() > max_len {
                    Err(CvarError::StringTooLong { name, max_len })
                } else {
                    Ok(CvarValue::String(text.clone()))
                }
            }
            (Self::Enum { variants }, CvarValue::Enum(candidate)) => variants
                .iter()
                .copied()
                .find(|variant| variant == candidate)
                .map(CvarValue::Enum)
                .ok_or_else(|| CvarError::InvalidEnumVariant { name, value: (*candidate).to_owned() }),
            (Self::Enum { variants }, CvarValue::String(candidate)) => variants
                .iter()
                .copied()
                .find(|variant| *variant == candidate.as_ref())
                .map(CvarValue::Enum)
                .ok_or_else(|| CvarError::InvalidEnumVariant { name, value: candidate.as_ref().to_owned() }),
            _ => Err(CvarError::TypeMismatch { name, expected: self.type_name() }),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CvarValue {
    Bool(bool),
    I64(i64),
    F32(f32),
    F64(f64),
    String(Cow<'static, str>),
    Enum(&'static str),
}

pub trait CvarTyped: Sized {
    fn from_value(value: &CvarValue) -> Option<Self>;
    fn into_cvar_value(self) -> CvarValue;
}

impl CvarTyped for bool {
    fn from_value(value: &CvarValue) -> Option<Self> {
        match value {
            CvarValue::Bool(flag) => Some(*flag),
            _ => None,
        }
    }

    fn into_cvar_value(self) -> CvarValue {
        CvarValue::Bool(self)
    }
}

impl CvarTyped for i64 {
    fn from_value(value: &CvarValue) -> Option<Self> {
        match value {
            CvarValue::I64(number) => Some(*number),
            _ => None,
        }
    }

    fn into_cvar_value(self) -> CvarValue {
        CvarValue::I64(self)
    }
}

impl CvarTyped for f32 {
    fn from_value(value: &CvarValue) -> Option<Self> {
        match value {
            CvarValue::F32(number) => Some(*number),
            _ => None,
        }
    }

    fn into_cvar_value(self) -> CvarValue {
        CvarValue::F32(self)
    }
}

impl CvarTyped for f64 {
    fn from_value(value: &CvarValue) -> Option<Self> {
        match value {
            CvarValue::F64(number) => Some(*number),
            _ => None,
        }
    }

    fn into_cvar_value(self) -> CvarValue {
        CvarValue::F64(self)
    }
}

impl CvarTyped for String {
    fn from_value(value: &CvarValue) -> Option<Self> {
        match value {
            CvarValue::String(text) => Some(text.as_ref().to_owned()),
            _ => None,
        }
    }

    fn into_cvar_value(self) -> CvarValue {
        CvarValue::String(Cow::Owned(self))
    }
}

impl CvarTyped for &'static str {
    fn from_value(value: &CvarValue) -> Option<Self> {
        match value {
            CvarValue::Enum(variant) => Some(*variant),
            _ => None,
        }
    }

    fn into_cvar_value(self) -> CvarValue {
        CvarValue::Enum(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GraphicsPreset {
    pub name: &'static str,
    pub entries: &'static [(&'static str, CvarValue)],
}

#[derive(Clone, Debug, PartialEq)]
pub struct CvarDescriptor {
    pub id: CvarId,
    pub name: &'static str,
    pub default: CvarValue,
    pub domain: ValueDomain,
    pub unit: Option<&'static str>,
    pub persistence: PersistenceScope,
    pub apply: ApplyScope,
    pub visibility: Visibility,
    pub help: &'static str,
}

impl CvarDescriptor {
    fn validate(&self, value: &CvarValue) -> Result<CvarValue, CvarError> {
        if let (CvarValue::I64(0), ValueDomain::I64 { min, max }, CvarValue::I64(0)) = (&self.default, self.domain, value)
            && !(min..=max).contains(&0)
        {
            return Ok(CvarValue::I64(0));
        }
        self.domain.validate(self.name, value)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct CvarRevision(u64);

impl CvarRevision {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CvarProvenance {
    pub value: CvarValue,
    pub source: Layer,
    pub pending_restart_value: Option<CvarValue>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CvarError {
    UnknownName(String),
    UnknownPreset(String),
    TypeMismatch { name: &'static str, expected: &'static str },
    OutOfBounds { name: &'static str },
    NonFinite { name: &'static str },
    InvalidEnumVariant { name: &'static str, value: String },
    StringTooLong { name: &'static str, max_len: usize },
    LayerNotAllowed { name: &'static str, layer: Layer },
}

impl fmt::Display for CvarError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownName(name) => write!(formatter, "unknown CVar `{name}`"),
            Self::UnknownPreset(name) => write!(formatter, "unknown graphics preset `{name}`"),
            Self::TypeMismatch { name, expected } => write!(formatter, "CVar `{name}` expects a `{expected}` value"),
            Self::OutOfBounds { name } => write!(formatter, "CVar `{name}` value is outside declared bounds"),
            Self::NonFinite { name } => write!(formatter, "CVar `{name}` value must be finite"),
            Self::InvalidEnumVariant { name, value } => write!(formatter, "CVar `{name}` does not support variant `{value}`"),
            Self::StringTooLong { name, max_len } => write!(formatter, "CVar `{name}` exceeds maximum length of {max_len} bytes"),
            Self::LayerNotAllowed { name, layer } => write!(formatter, "CVar `{name}` cannot be modified in layer `{layer:?}`"),
        }
    }
}

impl Error for CvarError {}

#[derive(Clone, Debug)]
struct SnapshotEntry {
    effective: CvarValue,
    source: Layer,
    pending_restart_value: Option<CvarValue>,
}

#[derive(Clone, Debug)]
pub struct CvarSnapshot {
    revision: CvarRevision,
    entries: Arc<[SnapshotEntry; CVAR_COUNT]>,
}

impl CvarSnapshot {
    pub const fn revision(&self) -> CvarRevision {
        self.revision
    }

    pub fn get<T: CvarTyped>(&self, key: CvarKey<T>) -> T {
        T::from_value(&self.entries[key.id.0].effective).expect("validated CVar value matches CvarKey type")
    }

    pub fn get_value(&self, id: CvarId) -> &CvarValue {
        &self.entries[id.0].effective
    }

    pub fn provenance(&self, id: CvarId) -> CvarProvenance {
        let entry = &self.entries[id.0];
        CvarProvenance { value: entry.effective.clone(), source: entry.source, pending_restart_value: entry.pending_restart_value.clone() }
    }

    pub fn pending_restarts(&self) -> Vec<(&'static CvarDescriptor, CvarValue)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.pending_restart_value.as_ref().map(|pending| (&DESCRIPTORS[index], pending.clone())))
            .collect()
    }
}

pub struct CvarRegistry {
    layers: [[Option<CvarValue>; CVAR_COUNT]; 4],
    startup_locked: [Option<(CvarValue, Layer)>; CVAR_COUNT],
    revision: CvarRevision,
    snapshot: CvarSnapshot,
}

impl Default for CvarRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CvarRegistry {
    pub fn new() -> Self {
        let layers: [[Option<CvarValue>; CVAR_COUNT]; 4] = array::from_fn(|_| array::from_fn(|_| None));
        let startup_locked: [Option<(CvarValue, Layer)>; CVAR_COUNT] = array::from_fn(|index| {
            let descriptor = &DESCRIPTORS[index];
            (descriptor.apply == ApplyScope::RestartRequired).then(|| (descriptor.default.clone(), Layer::Default))
        });
        let revision = CvarRevision(1);
        let snapshot = Self::build_snapshot(revision, &layers, &startup_locked);
        Self { layers, startup_locked, revision, snapshot }
    }

    pub const fn revision(&self) -> CvarRevision {
        self.revision
    }

    pub fn snapshot(&self) -> CvarSnapshot {
        self.snapshot.clone()
    }

    pub fn get<T: CvarTyped>(&self, key: CvarKey<T>) -> T {
        self.snapshot.get(key)
    }

    pub fn set<T: CvarTyped>(&mut self, key: CvarKey<T>, layer: Layer, value: T) -> Result<CvarRevision, CvarError> {
        self.set_value(key.id, layer, &value.into_cvar_value())
    }

    pub fn set_by_name(&mut self, name: &str, layer: Layer, value: &CvarValue) -> Result<CvarRevision, CvarError> {
        let descriptor = lookup_by_name(name).ok_or_else(|| CvarError::UnknownName(name.to_owned()))?;
        self.set_value(descriptor.id, layer, value)
    }

    pub fn set_value(&mut self, id: CvarId, layer: Layer, value: &CvarValue) -> Result<CvarRevision, CvarError> {
        let descriptor = id.descriptor();
        let slot = Self::validate_layer(descriptor, layer)?;
        let validated = descriptor.validate(value)?;
        let normalized = if layer == Layer::Global && validated == descriptor.default { None } else { Some(validated) };
        if self.layers[slot][id.0] != normalized {
            self.layers[slot][id.0] = normalized;
            self.commit_layer_changes();
        }
        Ok(self.revision)
    }

    pub fn clear_layer(&mut self, id: CvarId, layer: Layer) -> Result<CvarRevision, CvarError> {
        let descriptor = id.descriptor();
        let slot = Self::validate_layer(descriptor, layer)?;
        if self.layers[slot][id.0].take().is_some() {
            self.commit_layer_changes();
        }
        Ok(self.revision)
    }

    pub fn apply_batch(&mut self, layer: Layer, entries: &[(&str, CvarValue)]) -> Result<CvarRevision, CvarError> {
        let mut staged = Vec::with_capacity(entries.len());
        for (name, value) in entries {
            let descriptor = lookup_by_name(name).ok_or_else(|| CvarError::UnknownName((*name).to_owned()))?;
            let slot = Self::validate_layer(descriptor, layer)?;
            let validated = descriptor.validate(value)?;
            let normalized = if layer == Layer::Global && validated == descriptor.default { None } else { Some(validated) };
            staged.push((slot, descriptor.id.0, normalized));
        }
        let mut changed = false;
        for (slot, index, normalized) in staged {
            if self.layers[slot][index] != normalized {
                self.layers[slot][index] = normalized;
                changed = true;
            }
        }
        if changed {
            self.commit_layer_changes();
        }
        Ok(self.revision)
    }

    pub fn replace_layer(&mut self, layer: Layer, entries: &[(&str, CvarValue)]) -> Result<CvarRevision, CvarError> {
        let Some(slot) = layer.slot() else {
            return Err(CvarError::LayerNotAllowed { name: "", layer });
        };
        let mut next: [Option<CvarValue>; CVAR_COUNT] = array::from_fn(|_| None);
        for (name, value) in entries {
            let descriptor = lookup_by_name(name).ok_or_else(|| CvarError::UnknownName((*name).to_owned()))?;
            Self::validate_layer(descriptor, layer)?;
            let validated = descriptor.validate(value)?;
            let normalized = if layer == Layer::Global && validated == descriptor.default { None } else { Some(validated) };
            next[descriptor.id.0] = normalized;
        }
        if self.layers[slot] != next {
            self.layers[slot] = next;
            self.commit_layer_changes();
        }
        Ok(self.revision)
    }

    pub fn apply_preset(&mut self, preset: Option<&str>, presets: &[(&str, &[(&str, CvarValue)])]) -> Result<CvarRevision, CvarError> {
        match preset {
            None => self.replace_layer(Layer::Preset, &[]),
            Some(name) => {
                let (_, entries) =
                    presets.iter().find(|(candidate, _)| *candidate == name).ok_or_else(|| CvarError::UnknownPreset(name.to_owned()))?;
                self.replace_layer(Layer::Preset, entries)
            }
        }
    }

    pub fn apply_graphics_preset(&mut self, name: Option<&str>) -> Result<CvarRevision, CvarError> {
        match name {
            None => self.replace_layer(Layer::Preset, &[]),
            Some(name) => {
                let preset = graphics_preset(name).ok_or_else(|| CvarError::UnknownPreset(name.to_owned()))?;
                self.replace_layer(Layer::Preset, preset.entries)
            }
        }
    }

    pub fn layer_value(&self, id: CvarId, layer: Layer) -> Option<&CvarValue> {
        layer.slot().and_then(|slot| self.layers[slot][id.0].as_ref())
    }

    pub fn masking_layer(&self, id: CvarId, layer: Layer) -> Option<Layer> {
        Layer::OVERRIDE_LAYERS
            .into_iter()
            .rev()
            .find(|candidate| *candidate > layer && candidate.slot().is_some_and(|slot| self.layers[slot][id.0].is_some()))
    }

    pub fn lock_startup_values(&mut self) {
        for index in 0..CVAR_COUNT {
            let descriptor = &DESCRIPTORS[index];
            if descriptor.apply == ApplyScope::RestartRequired {
                self.startup_locked[index] = Some(Self::resolve_layers(&self.layers, index));
            }
        }
        self.snapshot = Self::build_snapshot(self.revision, &self.layers, &self.startup_locked);
    }

    fn validate_layer(descriptor: &'static CvarDescriptor, layer: Layer) -> Result<usize, CvarError> {
        let Some(slot) = layer.slot() else {
            return Err(CvarError::LayerNotAllowed { name: descriptor.name, layer });
        };
        if layer == Layer::Global && descriptor.persistence == PersistenceScope::SessionOnly {
            return Err(CvarError::LayerNotAllowed { name: descriptor.name, layer });
        }
        Ok(slot)
    }

    fn commit_layer_changes(&mut self) {
        let next_entries = Self::build_entries(&self.layers, &self.startup_locked);
        let observable_changed = self
            .snapshot
            .entries
            .iter()
            .zip(next_entries.iter())
            .any(|(before, after)| before.effective != after.effective || before.pending_restart_value != after.pending_restart_value);
        if observable_changed {
            self.revision = CvarRevision(self.revision.0.saturating_add(1));
        }
        self.snapshot = CvarSnapshot { revision: self.revision, entries: Arc::new(next_entries) };
    }

    fn resolve_layers(layers: &[[Option<CvarValue>; CVAR_COUNT]; 4], index: usize) -> (CvarValue, Layer) {
        for layer in Layer::OVERRIDE_LAYERS.into_iter().rev() {
            let slot = layer.slot().expect("override layer has slot");
            if let Some(value) = &layers[slot][index] {
                return (value.clone(), layer);
            }
        }
        (DESCRIPTORS[index].default.clone(), Layer::Default)
    }

    fn build_entries(
        layers: &[[Option<CvarValue>; CVAR_COUNT]; 4],
        startup_locked: &[Option<(CvarValue, Layer)>; CVAR_COUNT],
    ) -> [SnapshotEntry; CVAR_COUNT] {
        array::from_fn(|index| {
            let (resolved_value, resolved_layer) = Self::resolve_layers(layers, index);
            if let Some((locked_value, locked_layer)) = &startup_locked[index] {
                let pending_restart_value = (&resolved_value != locked_value).then_some(resolved_value);
                SnapshotEntry { effective: locked_value.clone(), source: *locked_layer, pending_restart_value }
            } else {
                SnapshotEntry { effective: resolved_value, source: resolved_layer, pending_restart_value: None }
            }
        })
    }

    fn build_snapshot(
        revision: CvarRevision,
        layers: &[[Option<CvarValue>; CVAR_COUNT]; 4],
        startup_locked: &[Option<(CvarValue, Layer)>; CVAR_COUNT],
    ) -> CvarSnapshot {
        CvarSnapshot { revision, entries: Arc::new(Self::build_entries(layers, startup_locked)) }
    }
}

pub fn descriptors() -> &'static [CvarDescriptor; CVAR_COUNT] {
    &DESCRIPTORS
}

pub fn graphics_preset(name: &str) -> Option<&'static GraphicsPreset> {
    GRAPHICS_PRESETS.iter().find(|preset| preset.name == name)
}

pub fn by_name(name: &str) -> Option<&'static CvarDescriptor> {
    lookup_by_name(name)
}

pub fn lookup_by_name(name: &str) -> Option<&'static CvarDescriptor> {
    DESCRIPTORS.iter().find(|descriptor| descriptor.name == name)
}

pub fn search(query: &str, include_hidden: bool) -> impl Iterator<Item = &'static CvarDescriptor> {
    search_in(&DESCRIPTORS, query, include_hidden)
}

fn search_in<'a>(descriptors: &'a [CvarDescriptor], query: &str, include_hidden: bool) -> impl Iterator<Item = &'a CvarDescriptor> {
    let needle = query.to_lowercase();
    descriptors.iter().filter(move |descriptor| {
        if !include_hidden && descriptor.visibility == Visibility::Hidden {
            return false;
        }
        if needle.is_empty() {
            return true;
        }
        descriptor.name.to_lowercase().contains(&needle) || descriptor.help.to_lowercase().contains(&needle)
    })
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

    #[test]
    fn defaults_load_without_io() {
        let registry = CvarRegistry::new();
        let snapshot = registry.snapshot();

        assert_eq!(RENDER_DRAW_DISTANCE.default_value(), 512.0);
        assert_eq!(SCENE_LOD_FACTOR.default_value(), 1.0);
        assert_eq!(MEMORY_MAX_VRAM_MIB.default_value(), 0);
        assert_eq!(MEMORY_VRAM_DIVISOR.default_value(), 1);
        assert_eq!(RENDER_MAX_TEXTURE_RESOLUTION.default_value(), "2048");
        assert_eq!(snapshot.get(RENDER_DRAW_DISTANCE), 512.0);
        assert_eq!(snapshot.get(SCENE_LOD_FACTOR), 1.0);
        assert_eq!(snapshot.get(MEMORY_MAX_VRAM_MIB), 0);
        assert_eq!(
            snapshot.provenance(RENDER_DRAW_DISTANCE.id()),
            CvarProvenance { value: CvarValue::F32(512.0), source: Layer::Default, pending_restart_value: None }
        );
        assert_eq!(lookup_by_name("render.draw_distance").map(|d| d.id), Some(RENDER_DRAW_DISTANCE.id()));
        assert_eq!(lookup_by_name("unknown.cvar"), None);
    }

    #[test]
    fn layer_precedence_is_deterministic() {
        let mut registry = CvarRegistry::new();
        let initial_revision = registry.revision();

        registry.set(RENDER_DRAW_DISTANCE, Layer::Preset, 256.0).expect("set preset");
        assert!(registry.revision() > initial_revision);
        assert_eq!(registry.get(RENDER_DRAW_DISTANCE), 256.0);
        assert_eq!(registry.snapshot().provenance(RENDER_DRAW_DISTANCE.id()).source, Layer::Preset);

        registry.set(RENDER_DRAW_DISTANCE, Layer::Global, 384.0).expect("set global");
        assert_eq!(registry.get(RENDER_DRAW_DISTANCE), 384.0);
        assert_eq!(registry.snapshot().provenance(RENDER_DRAW_DISTANCE.id()).source, Layer::Global);

        registry.set(RENDER_DRAW_DISTANCE, Layer::StartupDeveloper, 640.0).expect("set startup developer");
        assert_eq!(registry.get(RENDER_DRAW_DISTANCE), 640.0);
        assert_eq!(registry.snapshot().provenance(RENDER_DRAW_DISTANCE.id()).source, Layer::StartupDeveloper);

        registry.set(RENDER_DRAW_DISTANCE, Layer::Session, 128.0).expect("set session");
        assert_eq!(registry.get(RENDER_DRAW_DISTANCE), 128.0);
        assert_eq!(registry.snapshot().provenance(RENDER_DRAW_DISTANCE.id()).source, Layer::Session);

        registry.clear_layer(RENDER_DRAW_DISTANCE.id(), Layer::Session).expect("clear session");
        assert_eq!(registry.get(RENDER_DRAW_DISTANCE), 640.0);
        assert_eq!(registry.snapshot().provenance(RENDER_DRAW_DISTANCE.id()).source, Layer::StartupDeveloper);

        registry.clear_layer(RENDER_DRAW_DISTANCE.id(), Layer::StartupDeveloper).expect("clear startup developer");
        assert_eq!(registry.get(RENDER_DRAW_DISTANCE), 384.0);
        assert_eq!(registry.snapshot().provenance(RENDER_DRAW_DISTANCE.id()).source, Layer::Global);

        registry.set(RENDER_DRAW_DISTANCE, Layer::Global, 512.0).expect("setting global to default clears global override");
        assert_eq!(registry.get(RENDER_DRAW_DISTANCE), 256.0);
        assert_eq!(registry.snapshot().provenance(RENDER_DRAW_DISTANCE.id()).source, Layer::Preset);

        registry.clear_layer(RENDER_DRAW_DISTANCE.id(), Layer::Preset).expect("clear preset");
        assert_eq!(registry.get(RENDER_DRAW_DISTANCE), 512.0);
        assert_eq!(registry.snapshot().provenance(RENDER_DRAW_DISTANCE.id()).source, Layer::Default);
    }

    #[test]
    fn validation_rejects_invalid_values() {
        let mut registry = CvarRegistry::new();

        assert_matches!(
            registry.set(RENDER_DRAW_DISTANCE, Layer::Session, 16.0),
            Err(CvarError::OutOfBounds { name: "render.draw_distance" })
        );
        assert_matches!(
            registry.set(RENDER_DRAW_DISTANCE, Layer::Session, 2048.0),
            Err(CvarError::OutOfBounds { name: "render.draw_distance" })
        );
        assert_matches!(
            registry.set(RENDER_DRAW_DISTANCE, Layer::Session, f32::NAN),
            Err(CvarError::NonFinite { name: "render.draw_distance" })
        );
        assert_matches!(
            registry.set(RENDER_DRAW_DISTANCE, Layer::Session, f32::INFINITY),
            Err(CvarError::NonFinite { name: "render.draw_distance" })
        );
        assert_matches!(
            registry.set_by_name("render.draw_distance", Layer::Session, &CvarValue::I64(512)),
            Err(CvarError::TypeMismatch { name: "render.draw_distance", expected: "f32" })
        );
        assert_matches!(
            registry.set(RENDER_DRAW_DISTANCE, Layer::Default, 512.0),
            Err(CvarError::LayerNotAllowed { name: "render.draw_distance", layer: Layer::Default })
        );
        assert_matches!(
            registry.set(MEMORY_MAX_VRAM_MIB, Layer::Session, 1023),
            Err(CvarError::OutOfBounds { name: "memory.max_vram_mib" })
        );
        assert_matches!(registry.set(MEMORY_MAX_VRAM_MIB, Layer::Session, -1), Err(CvarError::OutOfBounds { name: "memory.max_vram_mib" }));
        assert_matches!(registry.set(RENDER_MAX_FPS, Layer::Session, 0), Err(CvarError::OutOfBounds { name: "render.max_fps" }));
        registry.set(MEMORY_MAX_VRAM_MIB, Layer::Session, 0).expect("auto zero is valid");
        registry.set(MEMORY_MAX_VRAM_MIB, Layer::Session, 1024).expect("range min is valid");
        registry.set(MEMORY_MAX_VRAM_MIB, Layer::Session, 65536).expect("range max is valid");
        assert_matches!(
            registry.set(RENDER_WORKER_THREADS, Layer::Session, 1),
            Err(CvarError::OutOfBounds { name: "render.worker_threads" })
        );
        registry.set(RENDER_WORKER_THREADS, Layer::Session, 0).expect("auto zero is valid");
        registry.set(RENDER_WORKER_THREADS, Layer::Session, 2).expect("range min is valid");
        registry.set(RENDER_WORKER_THREADS, Layer::Session, 64).expect("range max is valid");
        assert_matches!(
            registry.set(RENDER_MAX_TEXTURE_RESOLUTION, Layer::Session, "256"),
            Err(CvarError::InvalidEnumVariant { name: "render.max_texture_resolution", .. })
        );
        registry.set(RENDER_MAX_TEXTURE_RESOLUTION, Layer::Session, "512").expect("enum value is valid");
        assert_matches!(registry.set(MEMORY_MAX_RAM_MIB, Layer::Session, 255), Err(CvarError::OutOfBounds { name: "memory.max_ram_mib" }));
        registry.set(MEMORY_MAX_RAM_MIB, Layer::Session, 0).expect("auto zero is valid");
        registry.set(MEMORY_MAX_RAM_MIB, Layer::Session, 256).expect("range min is valid");
        registry.set(MEMORY_MAX_RAM_MIB, Layer::Session, 65536).expect("range max is valid");
        assert_matches!(
            registry.set(NETWORK_MAX_BANDWIDTH_KBPS, Layer::Session, 499),
            Err(CvarError::OutOfBounds { name: "network.max_bandwidth_kbps" })
        );
        registry.set(NETWORK_MAX_BANDWIDTH_KBPS, Layer::Session, 0).expect("auto zero is valid");
        registry.set(NETWORK_MAX_BANDWIDTH_KBPS, Layer::Session, 500).expect("range min is valid");
        registry.set(NETWORK_MAX_BANDWIDTH_KBPS, Layer::Session, 50000).expect("range max is valid");
        assert_eq!(registry.get(RENDER_DRAW_DISTANCE), 512.0);
    }

    #[test]
    fn batch_apply_is_atomic() {
        let mut registry = CvarRegistry::new();
        let before = registry.revision();

        let result = registry
            .apply_batch(Layer::Global, &[("render.draw_distance", CvarValue::F32(256.0)), ("scene.lod_factor", CvarValue::F32(99.0))]);
        assert_matches!(result, Err(CvarError::OutOfBounds { name: "scene.lod_factor" }));
        assert_eq!(registry.revision(), before);
        assert_eq!(registry.get(RENDER_DRAW_DISTANCE), 512.0);
        assert_eq!(registry.get(SCENE_LOD_FACTOR), 1.0);

        let after = registry
            .apply_batch(
                Layer::Global,
                &[
                    ("render.draw_distance", CvarValue::F32(256.0)),
                    ("scene.lod_factor", CvarValue::F32(2.0)),
                    ("memory.max_vram_mib", CvarValue::I64(2048)),
                ],
            )
            .expect("valid batch applies");
        assert!(after > before);
        assert_eq!(registry.get(RENDER_DRAW_DISTANCE), 256.0);
        assert_eq!(registry.get(SCENE_LOD_FACTOR), 2.0);
        assert_eq!(registry.get(MEMORY_MAX_VRAM_MIB), 2048);
    }

    #[test]
    fn restart_required_tracks_pending() {
        let mut registry = CvarRegistry::new();
        let id = RENDER_DRAW_DISTANCE.id();
        registry.startup_locked[id.0] = Some((CvarValue::F32(512.0), Layer::Default));

        registry.set(RENDER_DRAW_DISTANCE, Layer::Global, 256.0).expect("stage global override");
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.get(RENDER_DRAW_DISTANCE), 512.0);
        assert_eq!(
            snapshot.provenance(id),
            CvarProvenance { value: CvarValue::F32(512.0), source: Layer::Default, pending_restart_value: Some(CvarValue::F32(256.0)) }
        );

        registry.clear_layer(id, Layer::Global).expect("clear staged override");
        assert_eq!(registry.snapshot().provenance(id).pending_restart_value, None);
    }

    #[test]
    fn session_only_rejects_global_layer() {
        let mut registry = CvarRegistry::new();

        assert!(!RENDER_SHOW_STATS.default_value());
        assert!(!registry.get(RENDER_SHOW_STATS));
        assert_matches!(
            registry.set(RENDER_SHOW_STATS, Layer::Global, true),
            Err(CvarError::LayerNotAllowed { name: "render.show_stats", layer: Layer::Global })
        );
        registry.set(RENDER_SHOW_STATS, Layer::Session, true).expect("set session-only cvar in session layer");
        assert!(registry.get(RENDER_SHOW_STATS));
        assert_eq!(registry.snapshot().provenance(RENDER_SHOW_STATS.id()).source, Layer::Session);
    }

    #[test]
    fn enum_and_bool_validate() {
        let mut registry = CvarRegistry::new();

        assert_eq!(LOG_LEVEL.default_value(), "info");
        assert_eq!(registry.get(LOG_LEVEL), "info");
        assert!(!LOG_NOISY_NETWORK.default_value());
        assert!(!registry.get(LOG_NOISY_NETWORK));

        registry.set(LOG_LEVEL, Layer::Session, "debug").expect("set valid enum value");
        assert_eq!(registry.get(LOG_LEVEL), "debug");
        assert_matches!(
            registry.set(LOG_LEVEL, Layer::Session, "verbose"),
            Err(CvarError::InvalidEnumVariant { name: "log.level", value }) if value == "verbose"
        );
        assert_matches!(
            registry.set_by_name("log.level", Layer::Session, &CvarValue::String(Cow::Borrowed("invalid"))),
            Err(CvarError::InvalidEnumVariant { name: "log.level", value }) if value == "invalid"
        );
        assert_eq!(registry.get(LOG_LEVEL), "debug");

        registry.set(LOG_NOISY_NETWORK, Layer::Global, true).expect("set persistent bool in global layer");
        assert!(registry.get(LOG_NOISY_NETWORK));
        assert_eq!(registry.snapshot().provenance(LOG_NOISY_NETWORK.id()).source, Layer::Global);
    }

    #[test]
    fn noop_writes_keep_revision() {
        let mut registry = CvarRegistry::new();
        let initial = registry.revision();

        assert_eq!(registry.set(RENDER_DRAW_DISTANCE, Layer::Session, 512.0).expect("set same as default"), initial);
        assert_eq!(registry.set_by_name("render.draw_distance", Layer::Session, &CvarValue::F32(512.0)).expect("re-set same"), initial);
        assert_eq!(registry.clear_layer(RENDER_DRAW_DISTANCE.id(), Layer::Session).expect("clear to identical default"), initial);

        let after_session = registry.set(RENDER_DRAW_DISTANCE, Layer::Session, 256.0).expect("set session");
        assert!(after_session > initial);
        assert_eq!(registry.set(RENDER_DRAW_DISTANCE, Layer::Session, 256.0).expect("re-set identical session"), after_session);

        assert_eq!(registry.set(RENDER_DRAW_DISTANCE, Layer::Global, 384.0).expect("write masked global"), after_session);
        assert_eq!(registry.set(RENDER_DRAW_DISTANCE, Layer::Preset, 128.0).expect("write masked preset"), after_session);
        assert_eq!(
            registry.apply_batch(Layer::Global, &[("render.draw_distance", CvarValue::F32(640.0))]).expect("batch masked global"),
            after_session
        );
        assert_eq!(registry.clear_layer(RENDER_DRAW_DISTANCE.id(), Layer::Preset).expect("clear masked preset"), after_session);
        assert_eq!(registry.get(RENDER_DRAW_DISTANCE), 256.0);

        let after_clear_session = registry.clear_layer(RENDER_DRAW_DISTANCE.id(), Layer::Session).expect("clear session reveals global");
        assert!(after_clear_session > after_session);
        assert_eq!(registry.get(RENDER_DRAW_DISTANCE), 640.0);
    }

    #[test]
    fn search_respects_visibility() {
        assert_eq!(by_name("render.draw_distance").map(|descriptor| descriptor.id), Some(RENDER_DRAW_DISTANCE.id()));
        assert_eq!(by_name("does.not.exist"), None);

        let all_visible = search("", false).map(|descriptor| descriptor.name).collect::<Vec<_>>();
        assert_eq!(all_visible.len(), CVAR_COUNT);

        let by_name_match = search("DRAW_DIST", false).map(|descriptor| descriptor.name).collect::<Vec<_>>();
        assert_eq!(by_name_match, vec!["render.draw_distance"]);

        let by_help_match = search("EVICTION", false).map(|descriptor| descriptor.name).collect::<Vec<_>>();
        assert_eq!(by_help_match, vec!["memory.max_vram_mib"]);

        let synthetic = [
            CvarDescriptor {
                id: CvarId(0),
                name: "debug.visible_flag",
                default: CvarValue::Bool(false),
                domain: ValueDomain::Bool,
                unit: None,
                persistence: PersistenceScope::SessionOnly,
                apply: ApplyScope::Live,
                visibility: Visibility::Visible,
                help: "Visible diagnostic toggle.",
            },
            CvarDescriptor {
                id: CvarId(1),
                name: "debug.secret_flag",
                default: CvarValue::Bool(false),
                domain: ValueDomain::Bool,
                unit: None,
                persistence: PersistenceScope::SessionOnly,
                apply: ApplyScope::Live,
                visibility: Visibility::Hidden,
                help: "Hidden diagnostic toggle.",
            },
        ];

        let visible_only = search_in(&synthetic, "DIAGNOSTIC", false).map(|descriptor| descriptor.name).collect::<Vec<_>>();
        assert_eq!(visible_only, vec!["debug.visible_flag"]);

        let with_hidden = search_in(&synthetic, "DIAGNOSTIC", true).map(|descriptor| descriptor.name).collect::<Vec<_>>();
        assert_eq!(with_hidden, vec!["debug.visible_flag", "debug.secret_flag"]);

        let empty_visible = search_in(&synthetic, "", false).map(|descriptor| descriptor.name).collect::<Vec<_>>();
        assert_eq!(empty_visible, vec!["debug.visible_flag"]);

        let empty_with_hidden = search_in(&synthetic, "", true).map(|descriptor| descriptor.name).collect::<Vec<_>>();
        assert_eq!(empty_with_hidden, vec!["debug.visible_flag", "debug.secret_flag"]);
    }

    #[test]
    fn graphics_presets_apply() {
        let cases = [
            ("low", 128.0_f32, "512", 0.75_f32),
            ("medium", 256.0_f32, "1024", 1.0_f32),
            ("high", 384.0_f32, "2048", 1.5_f32),
            ("ultra", 512.0_f32, "2048", 2.0_f32),
        ];

        assert_eq!(GRAPHICS_PRESETS.len(), cases.len());
        let mut registry = CvarRegistry::new();

        for (name, draw_distance, max_texture_res, lod_factor) in cases {
            assert!(graphics_preset(name).is_some(), "missing preset `{name}`");
            registry.apply_graphics_preset(Some(name)).expect("apply bundled preset");
            assert_eq!(registry.get(RENDER_DRAW_DISTANCE), draw_distance);
            assert_eq!(registry.get(RENDER_MAX_TEXTURE_RESOLUTION), max_texture_res);
            assert_eq!(registry.get(MEMORY_MAX_VRAM_MIB), 0);
            assert_eq!(registry.get(SCENE_LOD_FACTOR), lod_factor);
            assert_eq!(registry.snapshot().provenance(RENDER_DRAW_DISTANCE.id()).source, Layer::Preset);
        }

        assert_matches!(registry.apply_graphics_preset(Some("nonexistent")), Err(CvarError::UnknownPreset(name)) if name == "nonexistent");

        registry.apply_graphics_preset(None).expect("clear graphics preset");
        assert_eq!(registry.get(RENDER_DRAW_DISTANCE), RENDER_DRAW_DISTANCE.default_value());
        assert_eq!(registry.snapshot().provenance(RENDER_DRAW_DISTANCE.id()).source, Layer::Default);
    }
}
