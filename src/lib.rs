//! hostctl: see at a glance whether every Nix host (NixOS, nix-darwin,
//! home-manager) runs the configuration its flake currently describes.
//! DESIGN.md describes the architecture.

pub mod actions;
pub mod cache;
pub mod closure;
pub mod config;
pub mod engine;
pub mod exec;
pub mod local;
pub mod model;
pub mod nix;
pub mod output;
pub mod probe;
pub mod remote;
pub mod storepath;
pub mod tui;
pub mod util;
