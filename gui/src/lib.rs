//! GUI support for the panoramic JS8 monitor.
//!
//! The pure pieces ([`colormap`], [`waterfall`]) have no windowing dependency
//! and are always available; the interactive [`app`] is behind the `desktop`
//! feature (eframe).

pub mod channels;
pub mod colormap;
pub mod demo;
pub mod diag;
pub mod layout;
pub mod qso;
pub mod record;
pub mod scene;
pub mod waterfall;

#[cfg(feature = "desktop")]
pub mod app;
#[cfg(feature = "desktop")]
pub mod audio;
#[cfg(feature = "desktop")]
pub mod tx;
