use std::{
    error,
    fmt::{self, Display, Formatter},
    fs::{self, File},
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use inventory::{
    InventoryCacheError, LoginLandmark, LoginLandmarkCache, LoginLandmarkImportSnapshot, MAX_LOGIN_LANDMARK_NAME_BYTES,
    MAX_LOGIN_LANDMARK_SLURL_BYTES, MAX_LOGIN_LANDMARKS_PER_AGENT,
};
use network::Grid;
use serde::Deserialize;
use serde_json::from_str;
use storage::sqlite::SqliteError;
use uuid::Uuid;

use super::{FavoriteLocation, favorites::FavoritesError};
use crate::session::name_cache::{AvatarId, NameCache, NameCacheError};

const APP_DIRECTORY: &str = "Radia";
const LOGIN_LOCATIONS_FILE: &str = "login_locations.json";
const IMPORT_SOURCE_KEY: &str = "login_locations.json";
const MAX_FAVORITE_STORE_BYTES: usize = 4 * 1024 * 1024;
const MAX_STORED_AGENTS: usize = 256;
pub(super) const MAX_FAVORITES_PER_AGENT: usize = MAX_LOGIN_LANDMARKS_PER_AGENT;
const MAX_USERNAME_BYTES: usize = 256;
const MAX_FAVORITE_NAME_BYTES: usize = MAX_LOGIN_LANDMARK_NAME_BYTES;
const MAX_REGION_NAME_BYTES: usize = 512;

#[derive(Clone, Debug)]
pub enum FavoriteStoreError {
    DataDirectoryUnavailable,
    Read { path: PathBuf, detail: String },
    FileTooLarge { path: PathBuf },
    InvalidFile { path: PathBuf, detail: String },
    InvalidStoredValues { path: PathBuf, reason: &'static str },
    InvalidRefreshedValues(&'static str),
    Cache(String),
    Favorites(FavoritesError),
}

impl Display for FavoriteStoreError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::DataDirectoryUnavailable => formatter.write_str("Could not find the roaming application-data directory."),
            Self::Read { path, detail } => write!(formatter, "Could not read login locations from {}: {detail}", path.display()),
            Self::FileTooLarge { path } => write!(formatter, "Saved login locations at {} exceed the 4 MiB limit.", path.display()),
            Self::InvalidFile { path, detail } => write!(formatter, "Saved login locations at {} are invalid: {detail}", path.display()),
            Self::InvalidStoredValues { path, reason } => {
                write!(formatter, "Saved login locations at {} are invalid: {reason}.", path.display())
            }
            Self::InvalidRefreshedValues(reason) => {
                write!(formatter, "Second Life returned favorite landmarks that cannot be stored: {reason}.")
            }
            Self::Cache(detail) => write!(formatter, "Favorite landmark cache failed: {detail}"),
            Self::Favorites(error) => Display::fmt(error, formatter),
        }
    }
}

impl error::Error for FavoriteStoreError {}

impl From<InventoryCacheError> for FavoriteStoreError {
    fn from(error: InventoryCacheError) -> Self {
        Self::Cache(error.to_string())
    }
}

impl From<NameCacheError> for FavoriteStoreError {
    fn from(error: NameCacheError) -> Self {
        Self::Cache(error.to_string())
    }
}

impl From<SqliteError> for FavoriteStoreError {
    fn from(error: SqliteError) -> Self {
        Self::Cache(error.to_string())
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct LegacyFavoriteStore {
    #[serde(default)]
    agents: Vec<LegacyAgentFavorites>,
}

#[derive(Clone, Debug, Deserialize)]
struct LegacyAgentFavorites {
    grid: LegacyGrid,
    agent_id: Uuid,
    username: String,
    #[serde(default)]
    favorites: Vec<FavoriteLocation>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum LegacyGrid {
    Agni,
    Aditi,
}

impl From<LegacyGrid> for Grid {
    fn from(grid: LegacyGrid) -> Self {
        match grid {
            LegacyGrid::Agni => Self::Agni,
            LegacyGrid::Aditi => Self::Aditi,
        }
    }
}

pub(crate) async fn import_legacy_default() -> Result<(), FavoriteStoreError> {
    let path = match login_locations_path() {
        Ok(path) => path,
        Err(FavoriteStoreError::DataDirectoryUnavailable) => return Ok(()),
        Err(error) => return Err(error),
    };
    let (landmarks, _) = LoginLandmarkCache::open()?;
    let (names, _) = NameCache::open_default()?;
    let result = import_legacy_from(&path, &landmarks, &names).await;
    if let Err(error) = landmarks.close() {
        tracing::debug!(%error, "landmark cache shutdown failed after legacy import");
    }
    if let Err(error) = names.shutdown() {
        tracing::debug!(%error, "name cache shutdown failed after legacy import");
    }
    result
}

pub(crate) async fn import_legacy_from(path: &Path, landmarks: &LoginLandmarkCache, names: &NameCache) -> Result<(), FavoriteStoreError> {
    if landmarks.has_import_marker(IMPORT_SOURCE_KEY).await? {
        return Ok(());
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(FavoriteStoreError::Read { path: path.to_owned(), detail: error.to_string() }),
    };

    let mut bytes = Vec::new();
    file.take((MAX_FAVORITE_STORE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| FavoriteStoreError::Read { path: path.to_owned(), detail: error.to_string() })?;
    if bytes.len() > MAX_FAVORITE_STORE_BYTES {
        return Err(FavoriteStoreError::FileTooLarge { path: path.to_owned() });
    }

    let contents =
        String::from_utf8(bytes).map_err(|error| FavoriteStoreError::InvalidFile { path: path.to_owned(), detail: error.to_string() })?;
    let legacy: LegacyFavoriteStore =
        from_str(&contents).map_err(|error| FavoriteStoreError::InvalidFile { path: path.to_owned(), detail: error.to_string() })?;

    let ValidatedLegacyImport { verified_logins, snapshots } =
        validate_legacy_store(&legacy).map_err(|reason| FavoriteStoreError::InvalidStoredValues { path: path.to_owned(), reason })?;

    if !verified_logins.is_empty() {
        names.seed_verified_logins(&verified_logins, current_timestamp_ms())?.wait().await?;
    }
    landmarks.import_if_absent(IMPORT_SOURCE_KEY, &snapshots).await?;

    if let Err(error) = fs::remove_file(path)
        && error.kind() != ErrorKind::NotFound
    {
        tracing::warn!(%error, path = %path.display(), "imported login_locations.json could not be removed");
    }
    Ok(())
}

pub(crate) async fn load_for_username_default(grid: Grid, username: &str) -> Vec<FavoriteLocation> {
    if username.trim().is_empty() {
        return Vec::new();
    }
    let Ok((names, _)) = NameCache::open_default() else {
        return Vec::new();
    };
    let Ok((landmarks, _)) = LoginLandmarkCache::open() else {
        if let Err(error) = names.shutdown() {
            tracing::debug!(%error, "name cache shutdown failed after landmark cache open error");
        }
        return Vec::new();
    };
    let result = load_for_username(&landmarks, &names, grid, username).await;
    if let Err(error) = landmarks.close() {
        tracing::debug!(%error, "landmark cache shutdown failed");
    }
    if let Err(error) = names.shutdown() {
        tracing::debug!(%error, "name cache shutdown failed");
    }
    match result {
        Ok(locations) => locations,
        Err(error) => {
            tracing::warn!(%error, "could not load cached login landmarks");
            Vec::new()
        }
    }
}

pub(crate) async fn load_for_username(
    landmarks: &LoginLandmarkCache,
    names: &NameCache,
    grid: Grid,
    username: &str,
) -> Result<Vec<FavoriteLocation>, FavoriteStoreError> {
    let Some(agent_id) = names.lookup_verified_agent_id(grid, username)?.wait().await? else {
        return Ok(Vec::new());
    };
    load_for_agent(landmarks, grid, agent_id.get()).await
}

pub(crate) async fn load_for_agent_default(grid: Grid, agent_id: Uuid) -> Vec<FavoriteLocation> {
    let Ok((landmarks, _)) = LoginLandmarkCache::open() else {
        return Vec::new();
    };
    let result = load_for_agent(&landmarks, grid, agent_id).await;
    if let Err(error) = landmarks.close() {
        tracing::debug!(%error, "landmark cache shutdown failed");
    }
    match result {
        Ok(locations) => locations,
        Err(error) => {
            tracing::warn!(%error, "could not load cached login landmarks for agent");
            Vec::new()
        }
    }
}

pub(crate) async fn load_for_agent(
    landmarks: &LoginLandmarkCache,
    grid: Grid,
    agent_id: Uuid,
) -> Result<Vec<FavoriteLocation>, FavoriteStoreError> {
    let rows = landmarks.load(grid.id(), agent_id).await?;
    Ok(rows.into_iter().filter_map(decode_landmark).collect())
}

pub(crate) async fn replace_for_agent_default(
    grid: Grid,
    agent_id: Uuid,
    favorites: &[FavoriteLocation],
) -> Result<(), FavoriteStoreError> {
    let (landmarks, _) = LoginLandmarkCache::open()?;
    let result = replace_for_agent(&landmarks, grid, agent_id, favorites).await;
    if let Err(error) = landmarks.close() {
        tracing::debug!(%error, "landmark cache shutdown failed");
    }
    result
}

pub(crate) async fn replace_for_agent(
    landmarks: &LoginLandmarkCache,
    grid: Grid,
    agent_id: Uuid,
    favorites: &[FavoriteLocation],
) -> Result<(), FavoriteStoreError> {
    let encoded = encode_favorites(favorites).map_err(FavoriteStoreError::InvalidRefreshedValues)?;
    landmarks.replace(grid.id(), agent_id, &encoded).await.map_err(Into::into)
}

pub(crate) fn validate_favorites(favorites: &[FavoriteLocation]) -> Result<(), &'static str> {
    encode_favorites(favorites).map(|_| ())
}

struct ValidatedLegacyImport {
    verified_logins: Vec<(Grid, String, AvatarId)>,
    snapshots: Vec<LoginLandmarkImportSnapshot>,
}

fn validate_legacy_store(legacy: &LegacyFavoriteStore) -> Result<ValidatedLegacyImport, &'static str> {
    if legacy.agents.len() > MAX_STORED_AGENTS {
        return Err("too many saved accounts");
    }
    let mut verified_logins = Vec::new();
    let mut snapshots = Vec::with_capacity(legacy.agents.len());
    for agent in &legacy.agents {
        let avatar_id = AvatarId::new(agent.agent_id).ok_or("a saved account has a nil agent id")?;
        if agent.username.len() > MAX_USERNAME_BYTES || agent.username.chars().any(char::is_control) {
            return Err("a saved username is invalid");
        }
        let landmarks = encode_favorites(&agent.favorites)?;
        let grid = Grid::from(agent.grid);
        if !agent.username.trim().is_empty() {
            verified_logins.push((grid, agent.username.clone(), avatar_id));
        }
        snapshots.push(LoginLandmarkImportSnapshot { grid_id: grid.id().to_owned(), agent_id: agent.agent_id, landmarks });
    }
    Ok(ValidatedLegacyImport { verified_logins, snapshots })
}

fn encode_favorites(favorites: &[FavoriteLocation]) -> Result<Vec<LoginLandmark>, &'static str> {
    if favorites.len() > MAX_FAVORITES_PER_AGENT {
        return Err("an account has too many saved favorites");
    }
    let mut encoded = Vec::with_capacity(favorites.len());
    for favorite in favorites {
        if favorite.name.is_empty() || favorite.name.len() > MAX_FAVORITE_NAME_BYTES || favorite.name.chars().any(char::is_control) {
            return Err("a saved favorite name is invalid");
        }
        if favorite.region_name.is_empty()
            || favorite.region_name.len() > MAX_REGION_NAME_BYTES
            || favorite.region_name.chars().any(char::is_control)
        {
            return Err("a saved favorite region name is invalid");
        }
        if favorite.x >= 256 || favorite.y >= 256 || favorite.z > 4096 {
            return Err("a saved favorite has out-of-range coordinates");
        }
        let slurl = encode_slurl(favorite);
        if slurl.len() > MAX_LOGIN_LANDMARK_SLURL_BYTES {
            return Err("a saved favorite SLURL is too long");
        }
        encoded.push(LoginLandmark { name: favorite.name.clone(), slurl });
    }
    Ok(encoded)
}

fn encode_slurl(location: &FavoriteLocation) -> String {
    let encoded_region = percent_encode(&location.region_name);
    format!("secondlife://{encoded_region}/{}/{}/{}", location.x, location.y, location.z)
}

fn decode_landmark(landmark: LoginLandmark) -> Option<FavoriteLocation> {
    if landmark.name.is_empty() || landmark.name.len() > MAX_FAVORITE_NAME_BYTES || landmark.name.chars().any(char::is_control) {
        return None;
    }
    let (region_name, x, y, z) = decode_slurl(&landmark.slurl)?;
    Some(FavoriteLocation { name: landmark.name, region_name, x, y, z })
}

fn decode_slurl(slurl: &str) -> Option<(String, u32, u32, u32)> {
    let rest = slurl.strip_prefix("secondlife://")?;
    let mut parts = rest.split('/');
    let encoded_region = parts.next()?;
    let x = parts.next()?.parse::<u32>().ok()?;
    let y = parts.next()?.parse::<u32>().ok()?;
    let z = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || x >= 256 || y >= 256 || z > 4096 {
        return None;
    }
    let region_name = percent_decode(encoded_region)?;
    if region_name.is_empty() || region_name.len() > MAX_REGION_NAME_BYTES || region_name.chars().any(char::is_control) {
        return None;
    }
    Some((region_name, x, y, z))
}

fn percent_encode(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(char::from(byte));
        } else {
            let high = hex_digit(byte >> 4);
            let low = hex_digit(byte & 0x0f);
            output.push('%');
            output.push(high);
            output.push(low);
        }
    }
    output
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => char::from(b'0' + nibble),
        10..=15 => char::from(b'A' + (nibble - 10)),
        _ => unreachable!(),
    }
}

fn percent_decode(input: &str) -> Option<String> {
    let mut bytes = input.as_bytes().iter().copied();
    let mut decoded = Vec::with_capacity(input.len());
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = from_hex(bytes.next()?)?;
            let low = from_hex(bytes.next()?)?;
            decoded.push((high << 4) | low);
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded).ok()
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn login_locations_path() -> Result<PathBuf, FavoriteStoreError> {
    let roaming_dir = dirs::data_dir().ok_or(FavoriteStoreError::DataDirectoryUnavailable)?;
    Ok(login_locations_path_under(&roaming_dir))
}

fn login_locations_path_under(data_dir: &Path) -> PathBuf {
    data_dir.join(APP_DIRECTORY).join(LOGIN_LOCATIONS_FILE)
}

fn current_timestamp_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, fs};

    use inventory::{InventoryCache, LoginLandmarkCache};
    use network::Grid;
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::{
        FavoriteStoreError, IMPORT_SOURCE_KEY, MAX_FAVORITE_NAME_BYTES, MAX_FAVORITE_STORE_BYTES, MAX_FAVORITES_PER_AGENT,
        import_legacy_from, load_for_agent, load_for_username, login_locations_path_under, replace_for_agent,
    };
    use crate::{
        inventory::FavoriteLocation,
        session::name_cache::{AvatarId, NameCache},
    };

    fn sample_location(name: &str, region_name: &str) -> FavoriteLocation {
        FavoriteLocation { name: name.into(), region_name: region_name.into(), x: 128, y: 64, z: 23 }
    }

    #[tokio::test]
    async fn import_seeds_and_removes_source() {
        let dir = tempdir().expect("tempdir");
        let json_path = login_locations_path_under(dir.path());
        fs::create_dir_all(json_path.parent().expect("parent")).expect("create parent");
        let agent_id = Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        let payload = format!(
            r#"{{"agents":[{{"grid":"agni","agent_id":"{agent_id}","username":"Example Resident","favorites":[{{"name":"Alpha","region_name":"Test Region","x":128,"y":64,"z":23}},{{"name":"Beta","region_name":"Ahern","x":1,"y":2,"z":3}}]}}]}}"#
        );
        fs::write(&json_path, payload).expect("write legacy json");

        let (landmarks, _) = LoginLandmarkCache::open_at_root(dir.path()).expect("open landmarks");
        let (names, _) = NameCache::open(dir.path().join("names.sqlite")).expect("open names");

        import_legacy_from(&json_path, &landmarks, &names).await.expect("import succeeds");

        assert!(!json_path.exists());
        assert!(landmarks.has_import_marker(IMPORT_SOURCE_KEY).await.expect("check marker"));
        let loaded = load_for_username(&landmarks, &names, Grid::Agni, "Example").await.expect("load by username");
        assert_eq!(
            loaded,
            vec![
                FavoriteLocation { name: "Alpha".into(), region_name: "Test Region".into(), x: 128, y: 64, z: 23 },
                FavoriteLocation { name: "Beta".into(), region_name: "Ahern".into(), x: 1, y: 2, z: 3 },
            ]
        );
        assert!(load_for_username(&landmarks, &names, Grid::Aditi, "Example").await.expect("other grid").is_empty());

        landmarks.close().expect("close landmarks");
        names.shutdown().expect("close names");
    }

    #[tokio::test]
    async fn interrupted_import_is_idempotent() {
        let dir = tempdir().expect("tempdir");
        let json_path = login_locations_path_under(dir.path());
        fs::create_dir_all(json_path.parent().expect("parent")).expect("create parent");
        let legacy_agent = Uuid::from_u128(10);
        let authoritative_agent = Uuid::from_u128(20);
        let payload = format!(
            r#"{{"agents":[{{"grid":"agni","agent_id":"{legacy_agent}","username":"Ada Lovelace","favorites":[{{"name":"Home","region_name":"Ahern","x":10,"y":20,"z":30}}]}}]}}"#
        );
        fs::write(&json_path, payload).expect("write legacy json");

        let (landmarks, _) = LoginLandmarkCache::open_at_root(dir.path()).expect("open landmarks");
        let (names, _) = NameCache::open(dir.path().join("names.sqlite")).expect("open names");

        names
            .seed_verified_logins(&[(Grid::Agni, "Ada Lovelace".into(), AvatarId::new(authoritative_agent).expect("non-nil"))], 100)
            .expect("seed authoritative")
            .wait()
            .await
            .expect("commit authoritative");

        import_legacy_from(&json_path, &landmarks, &names).await.expect("resume import");

        assert!(!json_path.exists());
        assert_eq!(
            names.lookup_verified_agent_id(Grid::Agni, "ada.lovelace").expect("queue").wait().await.expect("lookup"),
            AvatarId::new(authoritative_agent)
        );
        let legacy_loaded = load_for_agent(&landmarks, Grid::Agni, legacy_agent).await.expect("load legacy agent");
        assert_eq!(legacy_loaded, vec![FavoriteLocation { name: "Home".into(), region_name: "Ahern".into(), x: 10, y: 20, z: 30 }]);

        landmarks.close().expect("close landmarks");
        names.shutdown().expect("close names");
    }

    #[tokio::test]
    async fn import_preserves_snapshot_and_marker() {
        let dir = tempdir().expect("tempdir");
        let json_path = login_locations_path_under(dir.path());
        fs::create_dir_all(json_path.parent().expect("parent")).expect("create parent");
        let agent_id = Uuid::from_u128(30);
        let payload = format!(
            r#"{{"agents":[{{"grid":"agni","agent_id":"{agent_id}","username":"Example Resident","favorites":[{{"name":"Old","region_name":"Old Region","x":1,"y":2,"z":3}}]}}]}}"#
        );
        fs::write(&json_path, &payload).expect("write legacy json");

        let (landmarks, _) = LoginLandmarkCache::open_at_root(dir.path()).expect("open landmarks");
        let (names, _) = NameCache::open(dir.path().join("names.sqlite")).expect("open names");

        let existing = vec![sample_location("Current", "New Region")];
        replace_for_agent(&landmarks, Grid::Agni, agent_id, &existing).await.expect("write existing snapshot");

        import_legacy_from(&json_path, &landmarks, &names).await.expect("import with existing snapshot");
        assert_eq!(load_for_agent(&landmarks, Grid::Agni, agent_id).await.expect("load"), existing);

        replace_for_agent(&landmarks, Grid::Agni, agent_id, &[]).await.expect("empty refresh deletes rows");
        assert!(load_for_agent(&landmarks, Grid::Agni, agent_id).await.expect("load empty").is_empty());

        fs::write(&json_path, &payload).expect("simulate retained source file");
        import_legacy_from(&json_path, &landmarks, &names).await.expect("marker prevents reimport");
        assert!(load_for_agent(&landmarks, Grid::Agni, agent_id).await.expect("still empty").is_empty());

        landmarks.close().expect("close landmarks");
        names.shutdown().expect("close names");
    }

    #[tokio::test]
    async fn malformed_sources_stay_untouched() {
        let dir = tempdir().expect("tempdir");
        let (landmarks, _) = LoginLandmarkCache::open_at_root(dir.path()).expect("open landmarks");
        let (names, _) = NameCache::open(dir.path().join("names.sqlite")).expect("open names");

        let bad_json = dir.path().join("bad.json");
        fs::write(&bad_json, b"{ invalid json").expect("write bad json");
        let error = import_legacy_from(&bad_json, &landmarks, &names).await.expect_err("reject bad json");
        assert_matches!(error, FavoriteStoreError::InvalidFile { .. });
        assert!(bad_json.exists());
        assert!(!landmarks.has_import_marker(IMPORT_SOURCE_KEY).await.expect("marker absent"));

        let oversized = dir.path().join("oversized.json");
        fs::write(&oversized, vec![b' '; MAX_FAVORITE_STORE_BYTES + 1]).expect("write oversized");
        let error = import_legacy_from(&oversized, &landmarks, &names).await.expect_err("reject oversized");
        assert_matches!(error, FavoriteStoreError::FileTooLarge { .. });
        assert!(oversized.exists());

        let nil_agent = dir.path().join("nil.json");
        fs::write(
            &nil_agent,
            r#"{"agents":[{"grid":"agni","agent_id":"00000000-0000-0000-0000-000000000000","username":"A","favorites":[]}]}"#,
        )
        .expect("write nil agent");
        let error = import_legacy_from(&nil_agent, &landmarks, &names).await.expect_err("reject nil agent");
        assert_matches!(error, FavoriteStoreError::InvalidStoredValues { reason: "a saved account has a nil agent id", .. });
        assert!(nil_agent.exists());

        landmarks.close().expect("close landmarks");
        names.shutdown().expect("close names");
    }

    #[tokio::test]
    async fn ambiguous_username_skips_seed() {
        let dir = tempdir().expect("tempdir");
        let json_path = login_locations_path_under(dir.path());
        fs::create_dir_all(json_path.parent().expect("parent")).expect("create parent");
        let agent_a = Uuid::from_u128(40);
        let agent_b = Uuid::from_u128(41);
        let payload = format!(
            r#"{{"agents":[
                {{"grid":"agni","agent_id":"{agent_a}","username":"Shared User","favorites":[{{"name":"A","region_name":"Ahern","x":1,"y":2,"z":3}}]}},
                {{"grid":"agni","agent_id":"{agent_b}","username":"shared.user","favorites":[{{"name":"B","region_name":"Ahern","x":4,"y":5,"z":6}}]}}
            ]}}"#
        );
        fs::write(&json_path, payload).expect("write ambiguous json");

        let (landmarks, _) = LoginLandmarkCache::open_at_root(dir.path()).expect("open landmarks");
        let (names, _) = NameCache::open(dir.path().join("names.sqlite")).expect("open names");

        import_legacy_from(&json_path, &landmarks, &names).await.expect("import skips ambiguous username");
        assert!(load_for_username(&landmarks, &names, Grid::Agni, "Shared User").await.expect("ambiguous lookup").is_empty());

        let too_many = vec![sample_location("Home", "Ahern"); MAX_FAVORITES_PER_AGENT + 1];
        assert_matches!(
            replace_for_agent(&landmarks, Grid::Agni, agent_a, &too_many).await,
            Err(FavoriteStoreError::InvalidRefreshedValues("an account has too many saved favorites"))
        );
        let too_long = vec![sample_location(&"x".repeat(MAX_FAVORITE_NAME_BYTES + 1), "Ahern")];
        assert_matches!(
            replace_for_agent(&landmarks, Grid::Agni, agent_a, &too_long).await,
            Err(FavoriteStoreError::InvalidRefreshedValues("a saved favorite name is invalid"))
        );
        assert_eq!(load_for_agent(&landmarks, Grid::Agni, agent_a).await.expect("snapshot preserved").len(), 1);

        landmarks.close().expect("close landmarks");
        names.shutdown().expect("close names");
    }

    #[tokio::test]
    async fn clear_cache_clears_landmarks() {
        let dir = tempdir().expect("tempdir");
        let agent_id = Uuid::from_u128(50);
        let (landmarks, _) = LoginLandmarkCache::open_at_root(dir.path()).expect("open landmarks");
        replace_for_agent(&landmarks, Grid::Agni, agent_id, &[sample_location("Home", "Ahern")]).await.expect("store landmark");
        assert_eq!(load_for_agent(&landmarks, Grid::Agni, agent_id).await.expect("load").len(), 1);
        landmarks.close().expect("close landmarks");

        let (inv_cache, _) = InventoryCache::open_at_root(dir.path(), Grid::Agni.id(), agent_id, None).expect("open inventory cache");
        inv_cache.discard().await.expect("discard inventory cache");
        inv_cache.close().await.expect("close inventory cache");

        let (landmarks, _) = LoginLandmarkCache::open_at_root(dir.path()).expect("reopen landmarks");
        assert!(load_for_agent(&landmarks, Grid::Agni, agent_id).await.expect("cleared").is_empty());
        landmarks.close().expect("close landmarks");
    }
}
