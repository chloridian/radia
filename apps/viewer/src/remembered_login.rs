use std::{
    error,
    fmt::{self, Display, Formatter},
};

use network::Grid;
use prism::App;
use serde::{Deserialize, Serialize};
use serde_json::{from_slice, to_vec};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CredentialStoreError {
    Read(String),
    InvalidSavedData(String),
    Encode(String),
    Save(String),
    Remove(String),
}

impl Display for CredentialStoreError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(detail) => write!(formatter, "Could not read saved login: {detail}"),
            Self::InvalidSavedData(detail) => write!(formatter, "Saved login data is invalid: {detail}"),
            Self::Encode(detail) => write!(formatter, "Could not encode saved login: {detail}"),
            Self::Save(detail) => write!(formatter, "Could not save login to the system credential store: {detail}"),
            Self::Remove(detail) => write!(formatter, "Could not remove saved login: {detail}"),
        }
    }
}

impl error::Error for CredentialStoreError {}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub(crate) struct RememberedLogin {
    pub(crate) username: String,
    pub(crate) password: String,
    #[serde(default)]
    pub(crate) mfa_hash: String,
}

pub(crate) fn load(grid: Grid, cx: &App) -> impl Future<Output = Result<Option<RememberedLogin>, CredentialStoreError>> + use<> {
    let task = cx.read_credentials(grid_url(grid));
    async move {
        let Some((_, bytes)) = task.await.map_err(|error| CredentialStoreError::Read(error.to_string()))? else {
            return Ok(None);
        };
        decode(&bytes).map(Some)
    }
}

pub(crate) fn save(grid: Grid, login: &RememberedLogin, cx: &App) -> impl Future<Output = Result<(), CredentialStoreError>> + use<> {
    let task = encode(login).map(|bytes| cx.write_credentials(grid_url(grid), &login.username, &bytes));
    async move {
        let task = task?;
        task.await.map_err(|error| CredentialStoreError::Save(error.to_string()))
    }
}

pub(crate) fn clear(grid: Grid, cx: &App) -> impl Future<Output = Result<(), CredentialStoreError>> + use<> {
    let task = cx.delete_credentials(grid_url(grid));
    async move { task.await.map_err(|error| CredentialStoreError::Remove(error.to_string())) }
}

fn encode(login: &RememberedLogin) -> Result<Vec<u8>, CredentialStoreError> {
    to_vec(login).map_err(|error| CredentialStoreError::Encode(error.to_string()))
}

fn decode(bytes: &[u8]) -> Result<RememberedLogin, CredentialStoreError> {
    from_slice(bytes).map_err(|error| CredentialStoreError::InvalidSavedData(error.to_string()))
}

fn grid_url(grid: Grid) -> &'static str {
    match grid {
        Grid::Agni => "radia.login/agni",
        Grid::Aditi => "radia.login/aditi",
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use network::Grid;
    use prism::TestAppContext;

    use super::{CredentialStoreError, RememberedLogin, clear, decode, encode, grid_url, load, save};

    #[prism::test]
    async fn credential_store_operations_complete(cx: &mut TestAppContext) {
        let login = RememberedLogin { username: "Alice Resident".into(), password: "secret-password".into(), mfa_hash: "mfa-token".into() };

        assert_eq!(grid_url(Grid::Agni), "radia.login/agni");
        assert_eq!(grid_url(Grid::Aditi), "radia.login/aditi");
        assert_eq!(cx.update(|cx| save(Grid::Agni, &login, cx)).await, Ok(()));
        assert_eq!(cx.update(|cx| load(Grid::Agni, cx)).await, Ok(None));
        assert_eq!(cx.update(|cx| clear(Grid::Agni, cx)).await, Ok(()));
    }

    #[test]
    fn encode_and_decode_round_trip() {
        let login = RememberedLogin { username: "Alice Resident".into(), password: "secret-password".into(), mfa_hash: "mfa-token".into() };

        let bytes = encode(&login).expect("encode remembered login");
        assert_eq!(decode(&bytes), Ok(login));
    }

    #[test]
    fn invalid_saved_data_is_reported() {
        let error = match decode(b"not-valid-json") {
            Err(error) => error,
            Ok(_) => panic!("corrupt saved login must fail to decode"),
        };

        assert_matches!(&error, CredentialStoreError::InvalidSavedData(_));
        assert!(error.to_string().starts_with("Saved login data is invalid:"));
    }
}
