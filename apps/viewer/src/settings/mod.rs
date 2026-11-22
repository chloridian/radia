mod document;

use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, PoisonError},
};

use cvars::{
    ApplyScope, CvarDescriptor, CvarError, CvarId, CvarKey, CvarRegistry, CvarRevision, CvarSnapshot, CvarTyped, CvarValue,
    GRAPHICS_PRESETS, Layer, PersistenceScope, lookup_by_name,
};
use tokio::runtime::Handle;
use toml_edit::DocumentMut;
use utils::write_atomic;

use self::document::{FileFingerprint, fingerprint_on_disk, parse_document, read_bounded, set_document_preset, set_document_value};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettingsError {
    Io(String),
    FileTooLarge { max_bytes: usize },
    TooManyTables { max_tables: usize },
    TooManyKeys { max_keys: usize },
    Parse(String),
    InvalidValue(CvarError),
    Conflict,
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(detail) => write!(formatter, "settings I/O failed: {detail}"),
            Self::FileTooLarge { max_bytes } => write!(formatter, "settings file exceeds {max_bytes} bytes"),
            Self::TooManyTables { max_tables } => write!(formatter, "settings file exceeds {max_tables} tables"),
            Self::TooManyKeys { max_keys } => write!(formatter, "settings file exceeds {max_keys} keys"),
            Self::Parse(detail) => write!(formatter, "settings TOML parse failed: {detail}"),
            Self::InvalidValue(error) => write!(formatter, "invalid setting value: {error}"),
            Self::Conflict => formatter.write_str("settings file was modified externally; save aborted to preserve manual edits"),
        }
    }
}

impl Error for SettingsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidValue(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoadReport {
    pub unknown_keys: Vec<String>,
    pub preset: Option<String>,
    pub loaded_from_disk: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MutationOutcome {
    pub revision: CvarRevision,
    pub pending_restart: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResetOutcome {
    pub revision: CvarRevision,
    pub masked_by: Option<Layer>,
    pub pending_restart: bool,
}

struct SettingsState {
    registry: CvarRegistry,
    document: DocumentMut,
    active_preset: Option<String>,
    fingerprint: FileFingerprint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SaveWorker {
    Idle,
    Running,
    RunningDirty,
}

type NamedPresets = Vec<(String, Vec<(String, CvarValue)>)>;

pub struct SettingsStore {
    path: Option<PathBuf>,
    runtime: Option<Handle>,
    presets: NamedPresets,
    state: Mutex<SettingsState>,
    io_lock: Mutex<()>,
    save_worker: Mutex<SaveWorker>,
    save_idle: Condvar,
}

impl SettingsStore {
    pub fn in_memory() -> Self {
        Self {
            path: None,
            runtime: None,
            presets: bundled_presets(),
            state: Mutex::new(SettingsState {
                registry: CvarRegistry::new(),
                document: DocumentMut::new(),
                active_preset: None,
                fingerprint: FileFingerprint::ABSENT,
            }),
            io_lock: Mutex::new(()),
            save_worker: Mutex::new(SaveWorker::Idle),
            save_idle: Condvar::new(),
        }
    }

    pub fn load_or_default(path: PathBuf, runtime: Option<Handle>) -> (Self, Result<LoadReport, SettingsError>) {
        Self::load_or_default_with_presets(path, runtime, bundled_presets())
    }

    pub fn load_or_default_with_presets(
        path: PathBuf,
        runtime: Option<Handle>,
        presets: NamedPresets,
    ) -> (Self, Result<LoadReport, SettingsError>) {
        let store = Self {
            path: Some(path),
            runtime,
            presets,
            state: Mutex::new(SettingsState {
                registry: CvarRegistry::new(),
                document: DocumentMut::new(),
                active_preset: None,
                fingerprint: FileFingerprint::ABSENT,
            }),
            io_lock: Mutex::new(()),
            save_worker: Mutex::new(SaveWorker::Idle),
            save_idle: Condvar::new(),
        };
        let report = store.reload();
        (store, report)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn snapshot(&self) -> CvarSnapshot {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).registry.snapshot()
    }

    pub fn get<T: CvarTyped>(&self, key: CvarKey<T>) -> T {
        self.snapshot().get(key)
    }

    pub fn lock_startup_values(&self) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).registry.lock_startup_values();
    }

    pub fn pending_restarts(&self) -> Vec<(&'static CvarDescriptor, CvarValue)> {
        self.snapshot().pending_restarts()
    }

    pub fn reload(&self) -> Result<LoadReport, SettingsError> {
        let Some(path) = &self.path else {
            return Ok(LoadReport::default());
        };
        let _io_guard = self.io_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((bytes, fingerprint)) = read_bounded(path)? else {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.registry.apply_preset(None, &[]).map_err(SettingsError::InvalidValue)?;
            state.registry.replace_layer(Layer::Global, &[]).map_err(SettingsError::InvalidValue)?;
            state.document = DocumentMut::new();
            state.active_preset = None;
            state.fingerprint = FileFingerprint::ABSENT;
            return Ok(LoadReport::default());
        };

        let parsed = parse_document(&bytes)?;
        let borrowed = parsed.entries.iter().map(|(name, val)| (name.as_str(), val.clone())).collect::<Vec<_>>();
        let preset_slices = self
            .presets
            .iter()
            .map(|(name, entries)| (name.as_str(), entries.iter().map(|(key, val)| (key.as_str(), val.clone())).collect::<Vec<_>>()))
            .collect::<Vec<_>>();
        let preset_refs = preset_slices.iter().map(|(name, entries)| (*name, entries.as_slice())).collect::<Vec<_>>();

        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut candidate = CvarRegistry::new();
        candidate.apply_preset(parsed.preset.as_deref(), &preset_refs).map_err(SettingsError::InvalidValue)?;
        candidate.replace_layer(Layer::Global, &borrowed).map_err(SettingsError::InvalidValue)?;

        state.registry.apply_preset(parsed.preset.as_deref(), &preset_refs).map_err(SettingsError::InvalidValue)?;
        state.registry.replace_layer(Layer::Global, &borrowed).map_err(SettingsError::InvalidValue)?;
        state.document = parsed.document;
        state.active_preset = parsed.preset.clone();
        state.fingerprint = fingerprint;

        Ok(LoadReport { unknown_keys: parsed.unknown_keys, preset: parsed.preset, loaded_from_disk: true })
    }

    pub fn set_global<T: CvarTyped>(self: &Arc<Self>, key: CvarKey<T>, value: T) -> Result<MutationOutcome, SettingsError> {
        let outcome = self.apply_global_in_memory(key.id(), &value.into_cvar_value())?;
        self.schedule_save();
        Ok(outcome)
    }

    pub fn set_global_and_save<T: CvarTyped>(&self, key: CvarKey<T>, value: T) -> Result<MutationOutcome, SettingsError> {
        let outcome = self.apply_global_in_memory(key.id(), &value.into_cvar_value())?;
        self.save_sync()?;
        Ok(outcome)
    }

    pub fn set_session<T: CvarTyped>(&self, key: CvarKey<T>, value: T) -> Result<MutationOutcome, SettingsError> {
        let id = key.id();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let revision = state.registry.set_value(id, Layer::Session, &value.into_cvar_value()).map_err(SettingsError::InvalidValue)?;
        let pending_restart = state.registry.snapshot().provenance(id).pending_restart_value.is_some();
        Ok(MutationOutcome { revision, pending_restart })
    }

    pub fn set_startup_developer<T: CvarTyped>(&self, key: CvarKey<T>, value: T) -> Result<MutationOutcome, SettingsError> {
        let id = key.id();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let revision =
            state.registry.set_value(id, Layer::StartupDeveloper, &value.into_cvar_value()).map_err(SettingsError::InvalidValue)?;
        let pending_restart = state.registry.snapshot().provenance(id).pending_restart_value.is_some();
        Ok(MutationOutcome { revision, pending_restart })
    }

    pub fn reset_global(self: &Arc<Self>, id: CvarId) -> Result<ResetOutcome, SettingsError> {
        let outcome = self.reset_global_in_memory(id)?;
        self.schedule_save();
        Ok(outcome)
    }

    pub fn reset_global_and_save(&self, id: CvarId) -> Result<ResetOutcome, SettingsError> {
        let outcome = self.reset_global_in_memory(id)?;
        self.save_sync()?;
        Ok(outcome)
    }

    pub fn set_graphics_preset(&self, preset: Option<&str>) -> Result<MutationOutcome, SettingsError> {
        let preset_slices = self
            .presets
            .iter()
            .map(|(name, entries)| (name.as_str(), entries.iter().map(|(key, val)| (key.as_str(), val.clone())).collect::<Vec<_>>()))
            .collect::<Vec<_>>();
        let preset_refs = preset_slices.iter().map(|(name, entries)| (*name, entries.as_slice())).collect::<Vec<_>>();

        let outcome = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let revision = state.registry.apply_preset(preset, &preset_refs).map_err(SettingsError::InvalidValue)?;
            state.active_preset = preset.map(str::to_owned);
            set_document_preset(&mut state.document, preset);
            let pending_restart = !state.registry.snapshot().pending_restarts().is_empty();
            MutationOutcome { revision, pending_restart }
        };
        self.save_sync()?;
        Ok(outcome)
    }

    pub fn is_custom_preset(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(active) = state.active_preset.as_deref() else {
            return false;
        };
        let Some((_, entries)) = self.presets.iter().find(|(name, _)| name == active) else {
            return false;
        };
        entries.iter().any(|(key, preset_value)| {
            lookup_by_name(key)
                .and_then(|descriptor| state.registry.layer_value(descriptor.id, Layer::Global))
                .is_some_and(|global_value| global_value != preset_value)
        })
    }

    pub fn clear_preset_overrides(&self) -> Result<MutationOutcome, SettingsError> {
        let outcome = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(active) = state.active_preset.clone()
                && let Some((_, entries)) = self.presets.iter().find(|(name, _)| name == &active)
            {
                for (key, _) in entries {
                    if let Some(descriptor) = lookup_by_name(key) {
                        state.registry.clear_layer(descriptor.id, Layer::Global).map_err(SettingsError::InvalidValue)?;
                        set_document_value(&mut state.document, descriptor, None);
                    }
                }
            }
            let revision = state.registry.revision();
            let pending_restart = !state.registry.snapshot().pending_restarts().is_empty();
            MutationOutcome { revision, pending_restart }
        };
        self.save_sync()?;
        Ok(outcome)
    }

    pub fn save_sync(&self) -> Result<(), SettingsError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let _io_guard = self.io_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let (expected_fingerprint, serialized) = {
            let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            (state.fingerprint, state.document.to_string().into_bytes())
        };

        let disk_fingerprint = fingerprint_on_disk(path)?;
        if disk_fingerprint != expected_fingerprint {
            return Err(SettingsError::Conflict);
        }

        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|error| SettingsError::Io(error.to_string()))?;
        }
        write_atomic(path, &serialized).map_err(|error| SettingsError::Io(error.to_string()))?;

        let new_fingerprint = FileFingerprint::from_bytes(&serialized);
        self.state.lock().unwrap_or_else(PoisonError::into_inner).fingerprint = new_fingerprint;
        Ok(())
    }

    fn apply_global_in_memory(&self, id: CvarId, raw_value: &CvarValue) -> Result<MutationOutcome, SettingsError> {
        let descriptor = id.descriptor();
        if descriptor.persistence == PersistenceScope::SessionOnly {
            return Err(SettingsError::InvalidValue(CvarError::LayerNotAllowed { name: descriptor.name, layer: Layer::Global }));
        }
        let validated = descriptor.domain.validate(descriptor.name, raw_value).map_err(SettingsError::InvalidValue)?;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let revision = state.registry.set_value(id, Layer::Global, &validated).map_err(SettingsError::InvalidValue)?;
        let doc_value = (validated != descriptor.default).then_some(&validated);
        set_document_value(&mut state.document, descriptor, doc_value);
        let pending_restart =
            descriptor.apply == ApplyScope::RestartRequired && state.registry.snapshot().provenance(id).pending_restart_value.is_some();
        Ok(MutationOutcome { revision, pending_restart })
    }

    fn reset_global_in_memory(&self, id: CvarId) -> Result<ResetOutcome, SettingsError> {
        let descriptor = id.descriptor();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let revision = state.registry.clear_layer(id, Layer::Global).map_err(SettingsError::InvalidValue)?;
        set_document_value(&mut state.document, descriptor, None);
        let masked_by = state.registry.masking_layer(id, Layer::Global);
        let pending_restart = state.registry.snapshot().provenance(id).pending_restart_value.is_some();
        Ok(ResetOutcome { revision, masked_by, pending_restart })
    }

    pub fn wait_for_pending_save(&self) {
        let guard = self.save_worker.lock().unwrap_or_else(PoisonError::into_inner);
        let _guard = self.save_idle.wait_while(guard, |worker| *worker != SaveWorker::Idle).unwrap_or_else(PoisonError::into_inner);
    }

    fn schedule_save(self: &Arc<Self>) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let mut worker = self.save_worker.lock().unwrap_or_else(PoisonError::into_inner);
        match *worker {
            SaveWorker::Idle => *worker = SaveWorker::Running,
            SaveWorker::Running => {
                *worker = SaveWorker::RunningDirty;
                return;
            }
            SaveWorker::RunningDirty => return,
        }
        drop(worker);

        let store = Arc::clone(self);
        drop(runtime.spawn_blocking(move || {
            loop {
                if let Err(error) = store.save_sync() {
                    tracing::warn!(%error, "failed to save settings.toml");
                }
                let mut worker = store.save_worker.lock().unwrap_or_else(PoisonError::into_inner);
                match *worker {
                    SaveWorker::RunningDirty => *worker = SaveWorker::Running,
                    SaveWorker::Running | SaveWorker::Idle => {
                        *worker = SaveWorker::Idle;
                        store.save_idle.notify_all();
                        return;
                    }
                }
            }
        }));
    }
}

fn bundled_presets() -> NamedPresets {
    GRAPHICS_PRESETS
        .iter()
        .map(|preset| (preset.name.to_owned(), preset.entries.iter().map(|(key, val)| ((*key).to_owned(), val.clone())).collect()))
        .collect()
}

pub fn default_settings_path() -> io::Result<PathBuf> {
    let base = dirs::data_local_dir().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "local data directory unavailable"))?;
    Ok(base.join("Radia").join("settings.toml"))
}

#[cfg(test)]
mod tests {
    use std::{
        assert_matches, fs,
        sync::{Arc, PoisonError},
    };

    use cvars::{
        ASSET_DISK_CACHE_CAPACITY_MIB, CvarError, LOG_LEVEL, LOG_NOISY_NETWORK, Layer, RENDER_DRAW_DISTANCE, RENDER_SHOW_STATS,
        SCENE_LOD_FACTOR,
    };
    use tempfile::tempdir;
    use tokio::runtime::Builder;

    use super::{SaveWorker, SettingsError, SettingsStore};

    #[test]
    fn load_preserves_comments_and_unknown_keys_across_saves() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let original = "\
# User preferences
[render]
# Keep this comment
draw_distance = 256.0
experimental_flag = true

[custom]
note = \"keep me\"
";
        fs::write(&path, original).expect("write initial settings");

        let (store, report) = SettingsStore::load_or_default(path.clone(), None);
        let report = report.expect("valid settings load");
        assert!(report.loaded_from_disk);
        assert_eq!(report.unknown_keys, vec!["render.experimental_flag", "custom.note"]);
        assert_eq!(store.get(RENDER_DRAW_DISTANCE), 256.0);

        store.set_global_and_save(SCENE_LOD_FACTOR, 2.0).expect("save lod factor");
        let saved = fs::read_to_string(&path).expect("read saved settings");
        assert!(saved.contains("# User preferences"));
        assert!(saved.contains("# Keep this comment"));
        assert!(saved.contains("experimental_flag = true"));
        assert!(saved.contains("note = \"keep me\""));
        assert!(saved.contains("lod_factor = 2.0"));
    }

    #[test]
    fn invalid_known_value_rejects_candidate_atomically_and_leaves_file_untouched() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let malformed_value = "\
[render]
draw_distance = 256.0

[scene]
lod_factor = 999.0
";
        fs::write(&path, malformed_value).expect("write invalid settings");

        let (store, result) = SettingsStore::load_or_default(path.clone(), None);
        assert_matches!(result, Err(SettingsError::InvalidValue(CvarError::OutOfBounds { name: "scene.lod_factor" })));
        assert_eq!(store.get(RENDER_DRAW_DISTANCE), 512.0);
        assert_eq!(store.get(SCENE_LOD_FACTOR), 1.0);
        assert_eq!(fs::read_to_string(&path).expect("file untouched"), malformed_value);
    }

    #[test]
    fn reload_keeps_last_valid_config_on_malformed_file_or_unknown_preset() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        fs::write(&path, "[render]\ndraw_distance = 320.0\n").expect("write valid settings");

        let (store, result) = SettingsStore::load_or_default(path.clone(), None);
        result.expect("initial load");
        assert_eq!(store.get(RENDER_DRAW_DISTANCE), 320.0);

        fs::write(&path, "[render\nbroken toml").expect("write broken toml");
        assert_matches!(store.reload(), Err(SettingsError::Parse(_)));
        assert_eq!(store.get(RENDER_DRAW_DISTANCE), 320.0);

        fs::write(&path, "graphics_preset = \"imaginary\"\n[render]\ndraw_distance = 128.0\n").expect("write unknown preset");
        assert_matches!(store.reload(), Err(SettingsError::InvalidValue(CvarError::UnknownPreset(_))));
        assert_eq!(store.get(RENDER_DRAW_DISTANCE), 320.0);
    }

    #[test]
    fn external_edit_conflict_aborts_save_without_clobbering() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        fs::write(&path, "[render]\ndraw_distance = 256.0\n").expect("write initial");

        let (store, result) = SettingsStore::load_or_default(path.clone(), None);
        result.expect("initial load");

        let external = "[render]\ndraw_distance = 768.0\n";
        fs::write(&path, external).expect("external edit");

        assert_matches!(store.set_global_and_save(SCENE_LOD_FACTOR, 2.0), Err(SettingsError::Conflict));
        assert_eq!(fs::read_to_string(&path).expect("external edit preserved"), external);
    }

    #[test]
    fn reset_removes_only_target_key_and_reports_masked_higher_layer() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        fs::write(&path, "[render]\ndraw_distance = 256.0\n[scene]\nlod_factor = 2.0\n").expect("write initial");

        let (store, result) = SettingsStore::load_or_default(path.clone(), None);
        result.expect("initial load");
        store.set_session(RENDER_DRAW_DISTANCE, 128.0).expect("set session override");

        let reset = store.reset_global_and_save(RENDER_DRAW_DISTANCE.id()).expect("reset draw distance");
        assert_eq!(reset.masked_by, Some(Layer::Session));
        assert_eq!(store.get(RENDER_DRAW_DISTANCE), 128.0);
        assert_eq!(store.get(SCENE_LOD_FACTOR), 2.0);

        let saved = fs::read_to_string(&path).expect("read saved");
        assert!(!saved.contains("draw_distance"));
        assert!(saved.contains("lod_factor = 2.0"));
    }

    #[test]
    fn session_only_and_session_overrides_never_write_to_disk() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let (store, result) = SettingsStore::load_or_default(path.clone(), None);
        result.expect("absent file loads defaults");

        assert_matches!(
            store.set_global_and_save(RENDER_SHOW_STATS, true),
            Err(SettingsError::InvalidValue(CvarError::LayerNotAllowed { name: "render.show_stats", layer: Layer::Global }))
        );
        store.set_session(RENDER_SHOW_STATS, true).expect("session-only allowed in session layer");
        store.set_session(LOG_LEVEL, "debug").expect("session override");
        store.set_global_and_save(LOG_NOISY_NETWORK, true).expect("persist global override");

        let saved = fs::read_to_string(&path).expect("read saved");
        assert!(!saved.contains("show_stats"));
        assert!(!saved.contains("level"));
        assert!(saved.contains("noisy_network = true"));
    }

    #[test]
    fn restart_required_tracks_pending_restart_after_startup_lock() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let (store, result) = SettingsStore::load_or_default(path.clone(), None);
        result.expect("initial load");
        store.lock_startup_values();

        let outcome = store.set_global_and_save(ASSET_DISK_CACHE_CAPACITY_MIB, 2048).expect("stage restart-required override");
        assert!(outcome.pending_restart);
        assert_eq!(store.get(ASSET_DISK_CACHE_CAPACITY_MIB), 1024);
        assert_eq!(store.pending_restarts().len(), 1);
        assert_eq!(store.pending_restarts()[0].0.name, "asset.disk_cache_capacity_mib");

        let saved = fs::read_to_string(&path).expect("read saved");
        assert!(saved.contains("disk_cache_capacity_mib = 2048"));
    }

    #[test]
    fn oversize_settings_file_is_rejected_without_modification() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let oversized = vec![b'#'; super::document::MAX_SETTINGS_BYTES + 16];
        fs::write(&path, &oversized).expect("write oversized file");

        let (store, result) = SettingsStore::load_or_default(path.clone(), None);
        assert_matches!(result, Err(SettingsError::FileTooLarge { .. }));
        assert_eq!(store.get(RENDER_DRAW_DISTANCE), 512.0);
        assert_eq!(fs::read(&path).expect("file untouched"), oversized);
    }

    #[test]
    fn caller_supplied_presets_layer_below_global_and_higher_layers() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        fs::write(&path, "graphics_preset = \"medium\"\n[scene]\nlod_factor = 1.5\n").expect("write preset settings");

        let presets = vec![(
            "medium".to_owned(),
            vec![
                ("render.draw_distance".to_owned(), cvars::CvarValue::F32(256.0)),
                ("scene.lod_factor".to_owned(), cvars::CvarValue::F32(0.75)),
            ],
        )];
        let (store, report) = SettingsStore::load_or_default_with_presets(path, None, presets);
        let report = report.expect("preset settings load");
        assert_eq!(report.preset.as_deref(), Some("medium"));
        assert_eq!(store.get(RENDER_DRAW_DISTANCE), 256.0);
        assert_eq!(store.snapshot().provenance(RENDER_DRAW_DISTANCE.id()).source, Layer::Preset);
        assert_eq!(store.get(SCENE_LOD_FACTOR), 1.5);
        assert_eq!(store.snapshot().provenance(SCENE_LOD_FACTOR.id()).source, Layer::Global);

        store.set_startup_developer(RENDER_DRAW_DISTANCE, 640.0).expect("startup developer override");
        assert_eq!(store.get(RENDER_DRAW_DISTANCE), 640.0);
        assert_eq!(store.snapshot().provenance(RENDER_DRAW_DISTANCE.id()).source, Layer::StartupDeveloper);
    }

    #[test]
    fn bundled_presets_track_custom_overrides_and_clear_cleanly() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        fs::write(&path, "graphics_preset = \"medium\"\n[render]\ndraw_distance = 384.0\n").expect("write preset and override");

        let (store, report) = SettingsStore::load_or_default(path.clone(), None);
        let report = report.expect("bundled preset load");
        assert_eq!(report.preset.as_deref(), Some("medium"));
        assert_eq!(store.get(RENDER_DRAW_DISTANCE), 384.0);
        assert_eq!(store.snapshot().provenance(RENDER_DRAW_DISTANCE.id()).source, Layer::Global);
        assert!(store.is_custom_preset());

        store.reset_global_and_save(RENDER_DRAW_DISTANCE.id()).expect("reset global override");
        assert_eq!(store.get(RENDER_DRAW_DISTANCE), 256.0);
        assert_eq!(store.snapshot().provenance(RENDER_DRAW_DISTANCE.id()).source, Layer::Preset);
        assert!(!store.is_custom_preset());

        store.set_global_and_save(RENDER_DRAW_DISTANCE, 384.0).expect("re-apply explicit override");
        assert!(store.is_custom_preset());

        store.set_graphics_preset(Some("low")).expect("switch to low preset");
        assert_eq!(store.get(RENDER_DRAW_DISTANCE), 384.0);
        assert_eq!(store.get(SCENE_LOD_FACTOR), 0.75);
        assert!(store.is_custom_preset());
        let saved = fs::read_to_string(&path).expect("read saved settings");
        assert!(saved.contains("graphics_preset = \"low\""));
        assert!(saved.contains("draw_distance = 384.0"));

        store.clear_preset_overrides().expect("clear preset overrides");
        assert_eq!(store.get(RENDER_DRAW_DISTANCE), 128.0);
        assert_eq!(store.snapshot().provenance(RENDER_DRAW_DISTANCE.id()).source, Layer::Preset);
        assert!(!store.is_custom_preset());
        let cleared = fs::read_to_string(&path).expect("read cleared settings");
        assert!(cleared.contains("graphics_preset = \"low\""));
        assert!(!cleared.contains("draw_distance"));
    }

    #[test]
    fn burst_of_global_sets_runs_one_save_worker_and_persists_final_value() {
        let runtime = Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime");
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let (store, report) = SettingsStore::load_or_default(path.clone(), Some(runtime.handle().clone()));
        report.expect("initial load");
        let store = Arc::new(store);

        let io_guard = store.io_lock.lock().unwrap_or_else(PoisonError::into_inner);
        for i in 0..200_u16 {
            store.set_global(RENDER_DRAW_DISTANCE, 64.0 + f32::from(i)).expect("set draw distance");
        }
        assert_eq!(*store.save_worker.lock().unwrap_or_else(PoisonError::into_inner), SaveWorker::RunningDirty);
        drop(io_guard);

        store.wait_for_pending_save();
        assert_eq!(*store.save_worker.lock().unwrap_or_else(PoisonError::into_inner), SaveWorker::Idle);
        let saved = fs::read_to_string(&path).expect("read saved settings");
        assert!(saved.contains("draw_distance = 263.0"));
    }

    #[test]
    fn wait_for_pending_save_returns_when_idle() {
        let runtime = Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime");
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let (store, report) = SettingsStore::load_or_default(path, Some(runtime.handle().clone()));
        report.expect("initial load");
        store.wait_for_pending_save();
        assert_eq!(*store.save_worker.lock().unwrap_or_else(PoisonError::into_inner), SaveWorker::Idle);

        let in_memory = SettingsStore::in_memory();
        in_memory.wait_for_pending_save();
        assert_eq!(*in_memory.save_worker.lock().unwrap_or_else(PoisonError::into_inner), SaveWorker::Idle);
    }
}
