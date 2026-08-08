//! GUI support for the panoramic JS8 monitor.
//!
//! The pure pieces ([`colormap`], [`waterfall`]) have no windowing dependency
//! and are always available; the interactive [`app`] is behind the `desktop`
//! feature (eframe).

pub mod channels;
pub mod colormap;
pub mod continuous;
pub mod demo;
pub mod desktop;
pub mod diag;
pub mod icon;
pub mod layout;
pub mod qso;
pub mod record;
/// Keying a transmitter. Pure `std` — the rig is reached over a socket, so this
/// builds and is tested without the windowing or audio stack.
pub mod rig;
pub mod scene;
pub mod traffic;
pub mod waterfall;

#[cfg(feature = "desktop")]
pub mod app;
#[cfg(feature = "desktop")]
pub mod audio;
/// Routing another application's audio into the capture. Desktop-only: it is
/// about a live capture, which is a thing only the interactive app has.
#[cfg(feature = "desktop")]
pub mod pipewire;
/// A station's SNR history, drawn small. Desktop-only: it is a widget.
#[cfg(feature = "desktop")]
pub mod sparkline;
#[cfg(feature = "desktop")]
pub mod tx;
