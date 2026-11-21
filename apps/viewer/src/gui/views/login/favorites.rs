use std::{
    fmt::{self, Display, Formatter},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use futures::executor;
use network::Grid;
use prism::Context;
use uuid::Uuid;

use super::LoginScreen;
use crate::{
    inventory::{
        FavoriteLocation,
        favorite_store::{self, FavoriteStoreError},
    },
    session::{FavoritesUpdate, PresentationMailbox, ViewerUpdate},
};

#[derive(Clone, Debug)]
enum FavoriteWarning {
    Storage(FavoriteStoreError),
    Save(FavoriteStoreError),
    Refresh(FavoriteStoreError),
}

impl Display for FavoriteWarning {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => Display::fmt(error, formatter),
            Self::Save(error) => write!(formatter, "Could not save favorite landmarks: {error}"),
            Self::Refresh(error) => write!(formatter, "Could not refresh favorite landmarks: {error}"),
        }
    }
}

pub(super) struct Favorites {
    locations: Vec<FavoriteLocation>,
    displayed: Vec<FavoriteLocation>,
    storage_warning: Option<FavoriteWarning>,
    refresh_warning: Option<FavoriteWarning>,
    lookup_generation: u64,
    save_generation: Arc<AtomicU64>,
}

impl Favorites {
    pub(super) fn new() -> Self {
        Self {
            locations: Vec::new(),
            displayed: Vec::new(),
            storage_warning: None,
            refresh_warning: None,
            lookup_generation: 0,
            save_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(super) fn apply_import_result(&mut self, result: Result<(), FavoriteStoreError>) {
        self.storage_warning = result.err().map(FavoriteWarning::Storage);
    }

    pub(super) fn locations(&self) -> &[FavoriteLocation] {
        &self.locations
    }

    pub(super) fn needs_display_refresh(&self) -> bool {
        self.displayed != self.locations
    }

    pub(super) fn mark_displayed(&mut self) {
        self.displayed.clone_from(&self.locations);
    }

    pub(super) fn set_locations(&mut self, locations: Vec<FavoriteLocation>) {
        self.locations.clone_from(&locations);
        self.displayed = locations;
    }

    pub(super) fn next_lookup_generation(&mut self) -> u64 {
        self.lookup_generation = self.lookup_generation.wrapping_add(1);
        self.lookup_generation
    }

    pub(super) fn apply_lookup_result(&mut self, generation: u64, locations: Vec<FavoriteLocation>) -> bool {
        if generation != self.lookup_generation || locations == self.locations {
            return false;
        }
        self.locations = locations;
        true
    }

    pub(super) fn prepare_session(&mut self, grid: Grid, agent_id: Uuid, session_generation: u64, cx: &mut Context<LoginScreen>) {
        self.refresh_warning = None;
        let background = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let locations =
                background.spawn(async move { executor::block_on(favorite_store::load_for_agent_default(grid, agent_id)) }).await;
            if let Err(error) = this.update(cx, |screen, cx| {
                if screen.session_generation != session_generation {
                    return;
                }
                if screen.favorites.locations != locations {
                    screen.favorites.locations = locations;
                    cx.notify();
                }
            }) {
                tracing::debug!(%error, "login screen dropped before session favorites loaded");
            }
        })
        .detach();
    }

    pub(super) fn apply_update(&mut self, update: FavoritesUpdate, mailbox: Option<PresentationMailbox>, cx: &mut Context<LoginScreen>) {
        let favorites = match update.result {
            Ok(favorites) => favorites,
            Err(error) => {
                let error = FavoriteStoreError::Favorites(error);
                tracing::warn!(%error, "could not apply favorite inventory refresh");
                self.refresh_warning = Some(FavoriteWarning::Refresh(error));
                return;
            }
        };
        if let Err(reason) = favorite_store::validate_favorites(&favorites) {
            let error = FavoriteStoreError::InvalidRefreshedValues(reason);
            tracing::warn!(%error, "could not apply favorite inventory refresh");
            self.refresh_warning = Some(FavoriteWarning::Refresh(error));
            return;
        }

        self.locations.clone_from(&favorites);
        self.refresh_warning = None;
        self.storage_warning = None;
        self.schedule_save(mailbox, update.grid, update.agent_id, favorites, cx);
    }

    pub(super) fn apply_saved(&mut self, saved: Option<(u64, Result<(), FavoriteStoreError>)>) {
        let Some((generation, result)) = saved else { return };
        if generation != self.save_generation.load(Ordering::Acquire) {
            return;
        }

        match result {
            Ok(()) => self.storage_warning = None,
            Err(error) => {
                tracing::warn!(%error, "could not save favorite landmarks");
                self.storage_warning = Some(FavoriteWarning::Save(error));
            }
        }
    }

    pub(super) fn status_messages(&self) -> Vec<String> {
        self.storage_warning.iter().chain(self.refresh_warning.iter()).map(ToString::to_string).collect()
    }

    fn schedule_save(
        &self,
        mailbox: Option<PresentationMailbox>,
        grid: Grid,
        agent_id: Uuid,
        favorites: Vec<FavoriteLocation>,
        cx: &mut Context<LoginScreen>,
    ) {
        let Some(mailbox) = mailbox else { return };
        let save_generation = Arc::clone(&self.save_generation);
        let generation = save_generation.fetch_add(1, Ordering::AcqRel) + 1;
        cx.background_executor()
            .spawn(async move {
                if save_generation.load(Ordering::Acquire) != generation {
                    return;
                }
                let result = executor::block_on(favorite_store::replace_for_agent_default(grid, agent_id, &favorites));
                mailbox.publish(ViewerUpdate::FavoriteStoreSaved { generation, result });
            })
            .detach();
    }
}
