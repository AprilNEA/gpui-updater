//! Platforms without Sparkle: [`Driver::load`] always declines, so a
//! `SparkleDriver` can never be constructed and every method is unreachable.

use std::convert::Infallible;

use gpui_updater_core::DriverEvents;

use crate::error::Error;
use crate::options::SparkleOptions;

#[derive(Debug)]
pub(crate) struct Driver {
    never: Infallible,
}

impl Driver {
    #[expect(
        clippy::unnecessary_wraps,
        reason = "mirrors the macOS driver, which can fail"
    )]
    pub(crate) fn load(options: SparkleOptions) -> Result<Option<Self>, Error> {
        drop(options);
        Ok(None)
    }

    pub(crate) fn start(&mut self, _events: DriverEvents) {
        match self.never {}
    }

    pub(crate) fn check(&mut self) {
        match self.never {}
    }

    pub(crate) fn install(&mut self) {
        match self.never {}
    }

    pub(crate) fn relaunch(&mut self) {
        match self.never {}
    }

    pub(crate) fn can_check_for_updates(&self) -> bool {
        match self.never {}
    }

    pub(crate) fn check_in_background(&self) {
        match self.never {}
    }

    pub(crate) fn check_for_update_information(&self) {
        match self.never {}
    }

    pub(crate) fn automatically_checks_for_updates(&self) -> Option<bool> {
        match self.never {}
    }

    pub(crate) fn set_automatically_checks_for_updates(&self, _enabled: bool) {
        match self.never {}
    }

    pub(crate) fn automatically_downloads_updates(&self) -> Option<bool> {
        match self.never {}
    }

    pub(crate) fn set_automatically_downloads_updates(&self, _enabled: bool) {
        match self.never {}
    }
}
